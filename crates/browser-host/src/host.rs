//! The Servo thread's side: the pages, what they tell their apps, and their frames.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use dpi::PhysicalSize;
use euclid::Scale;
use protocol::BusError;
use protocol::bus::code;
use protocol::surface::{self as wire, AttachParams, InputParams, ReleaseParams, SurfaceInput};
use protocol::web::{self, Load, NavigateParams, OpenParams, PageEvent, PageParams};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use servo::{
    Code, DeviceIntRect, DevicePoint, InputEvent, Key, KeyState, KeyboardEvent, LoadStatus, Location,
    Modifiers, MouseButton, MouseButtonAction, MouseButtonEvent, MouseMoveEvent, NamedKey, RenderingContext,
    Servo, ServoBuilder, SoftwareRenderingContext, WebView, WebViewBuilder, WebViewDelegate, WebViewPoint,
    WheelDelta, WheelEvent, WheelMode,
};
use url::Url;

use crate::surface::{Surface, checked};
use crate::{Out, Waker, error};

/// The size a page is laid out at before anything shows it.
const START_SIZE: (u32, u32) = (1024, 768);

/// What Servo tells us about one page, kept until the Servo thread passes it on.
#[derive(Default)]
struct Delegate {
    events: RefCell<Vec<PageEvent>>,
    frame: Cell<bool>,
    crashed: RefCell<Option<String>>,
}

impl WebViewDelegate for Delegate {
    fn notify_new_frame_ready(&self, _: WebView) {
        self.frame.set(true);
    }
    fn notify_url_changed(&self, _: WebView, url: Url) {
        self.events.borrow_mut().push(PageEvent::Url { url: url.to_string() });
    }
    fn notify_page_title_changed(&self, _: WebView, title: Option<String>) {
        self.events.borrow_mut().push(PageEvent::Title { title });
    }
    fn notify_load_status_changed(&self, _: WebView, status: LoadStatus) {
        let load = match status {
            LoadStatus::Started => Load::Started,
            LoadStatus::HeadParsed => Load::Parsed,
            LoadStatus::Complete => Load::Complete,
            // Stopped (window.stop()): as far as it'll get.
            _ => Load::Complete,
        };
        self.events.borrow_mut().push(PageEvent::Load { load });
    }
    fn notify_history_changed(&self, _: WebView, entries: Vec<Url>, current: usize) {
        let (back, forward) = (current > 0, current + 1 < entries.len());
        self.events.borrow_mut().push(PageEvent::History { back, forward });
    }
    fn notify_crashed(&self, _: WebView, reason: String, _backtrace: Option<String>) {
        *self.crashed.borrow_mut() = Some(reason);
    }
}

struct Page {
    /// Who opened it (`app:ID`): only they may steer it or show it.
    owner: String,
    /// The `open` request, whose stream the page's events go out on.
    open: u64,
    view: WebView,
    rc: Rc<SoftwareRenderingContext>,
    delegate: Rc<Delegate>,
    surface: Option<Surface>,
}

pub struct Host {
    servo: Servo,
    pages: HashMap<String, Page>,
    next: u64,
    out: tokio::sync::mpsc::UnboundedSender<Out>,
}

fn parse<T: DeserializeOwned>(params: Value) -> Result<T, BusError> {
    serde_json::from_value(params).map_err(|e| error(code::BAD_REQUEST, e.to_string()))
}

impl Host {
    pub fn new(waker: Waker, out: tokio::sync::mpsc::UnboundedSender<Out>) -> Host {
        let servo = ServoBuilder::default().event_loop_waker(Box::new(waker)).build();
        Host { servo, pages: HashMap::new(), next: 1, out }
    }

    fn send(&self, id: u64, data: impl serde::Serialize) {
        let data = serde_json::to_value(data).expect("serializes");
        let _ = self.out.send(Out::Part { id, data, fds: Vec::new() });
    }

    fn respond(&self, id: u64, result: Result<Value, BusError>) {
        let _ = self.out.send(Out::Respond { id, result });
    }

    /// Request `id` from `caller`.
    pub fn request(&mut self, id: u64, caller: &str, method: &str, params: Value) {
        let result = match method {
            web::method::OPEN => match self.open(id, caller, params) {
                // Answered with parts until the page closes.
                Ok(()) => return,
                Err(e) => Err(e),
            },
            wire::method::ATTACH => match self.attach(id, caller, params) {
                Ok(()) => return,
                Err(e) => Err(e),
            },
            wire::method::RESIZE => self.resize(caller, params),
            wire::method::RELEASE => parse::<ReleaseParams>(params).and_then(|p| {
                let page = self.page(caller, &p.id)?;
                if let Some(s) = &mut page.surface {
                    s.release(p.buffer, p.seq);
                }
                Ok(Value::Null)
            }),
            wire::method::INPUT => parse::<InputParams>(params).and_then(|p| {
                let page = self.page(caller, &p.id)?;
                match input(p.event) {
                    Steer::Event(e) => {
                        page.view.notify_input_event(e);
                    }
                    Steer::Focus(on) => page.view.set_focused(on),
                }
                Ok(Value::Null)
            }),
            web::method::NAVIGATE => parse::<NavigateParams>(params).and_then(|p| {
                let url = web::fix_address(&p.url)
                    .and_then(|u| Url::parse(&u).ok())
                    .ok_or_else(|| error(code::BAD_REQUEST, "not an address"))?;
                self.page(caller, &p.id)?.view.load(url);
                Ok(Value::Null)
            }),
            web::method::BACK | web::method::FORWARD | web::method::RELOAD => parse::<PageParams>(params)
                .and_then(|p| {
                    let view = &self.page(caller, &p.id)?.view;
                    match method {
                        web::method::BACK => drop(view.go_back(1)),
                        web::method::FORWARD => drop(view.go_forward(1)),
                        _ => view.reload(),
                    }
                    Ok(Value::Null)
                }),
            other => Err(error(code::BAD_REQUEST, format!("web has no method {other:?}"))),
        };
        self.respond(id, result);
    }

    /// The page `id`, if it's `caller`'s.
    fn page(&mut self, caller: &str, id: &str) -> Result<&mut Page, BusError> {
        match self.pages.get_mut(id) {
            Some(p) if p.owner == caller => Ok(p),
            Some(_) => Err(error(code::PERMISSION_DENIED, "that page isn't yours")),
            None => Err(error(code::BAD_REQUEST, format!("no page {id:?}"))),
        }
    }

    fn open(&mut self, req: u64, caller: &str, params: Value) -> Result<(), BusError> {
        let p: OpenParams = parse(params)?;
        // Apps, and tools (an app run with `app-host --bundle`): a service has no
        // window to show a page in.
        if !(caller.starts_with("app:") || caller.starts_with("conn:")) {
            return Err(error(code::PERMISSION_DENIED, "pages are opened by apps"));
        }
        if self.pages.len() >= web::MAX_PAGES {
            return Err(error(code::LIMIT_EXCEEDED, format!("{} pages are open already", web::MAX_PAGES)));
        }
        let url = match p.url.as_deref() {
            Some(typed) => {
                web::fix_address(typed).ok_or_else(|| error(code::BAD_REQUEST, "not an address"))?
            }
            None => "about:blank".to_owned(),
        };
        let url = Url::parse(&url).map_err(|e| error(code::BAD_REQUEST, e.to_string()))?;
        let rc = SoftwareRenderingContext::new(PhysicalSize::new(START_SIZE.0, START_SIZE.1))
            .map_err(|e| error("unavailable", format!("no GL for a page: {e:?}")))?;
        let rc = Rc::new(rc);
        let _ = rc.make_current();
        let delegate = Rc::new(Delegate::default());
        let view = WebViewBuilder::new(&self.servo, rc.clone()).url(url).delegate(delegate.clone()).build();
        let id = self.next.to_string();
        self.next += 1;
        log::info!("browser-host: page {id} opened for {caller}");
        self.pages.insert(
            id.clone(),
            Page { owner: caller.to_owned(), open: req, view, rc, delegate, surface: None },
        );
        self.send(req, PageEvent::Opened { id });
        Ok(())
    }

    fn attach(&mut self, req: u64, caller: &str, params: Value) -> Result<(), BusError> {
        let p: AttachParams = parse(params)?;
        let (w, h, scale) = checked(p.width, p.height, p.scale).map_err(|e| error(code::BAD_REQUEST, e))?;
        let page = self.page(caller, &p.id)?;
        // A page is shown in one place: a new attachment ends the old.
        let old = page.surface.replace(Surface::new(req)).map(|s| s.req);
        let sized = size(page, w, h, scale);
        if let Some(old) = old {
            self.respond(old, Ok(Value::Null));
        }
        let (part, fds) = sized?;
        let _ = self.out.send(Out::Part { id: req, data: part, fds });
        Ok(())
    }

    fn resize(&mut self, caller: &str, params: Value) -> Result<Value, BusError> {
        let p: AttachParams = parse(params)?;
        let (w, h, scale) = checked(p.width, p.height, p.scale).map_err(|e| error(code::BAD_REQUEST, e))?;
        let page = self.page(caller, &p.id)?;
        let Some(req) = page.surface.as_ref().map(|s| s.req) else {
            return Err(error(code::BAD_REQUEST, "that page isn't attached"));
        };
        let (part, fds) = size(page, w, h, scale)?;
        let _ = self.out.send(Out::Part { id: req, data: part, fds });
        Ok(Value::Null)
    }

    /// Request `id` ended: an `open` closes its page, an `attach` its surface.
    pub fn cancel(&mut self, id: u64) {
        if let Some(pid) = self.pages.iter().find(|(_, p)| p.open == id).map(|(k, _)| k.clone()) {
            self.close(&pid);
            return;
        }
        for page in self.pages.values_mut() {
            if page.surface.as_ref().is_some_and(|s| s.req == id) {
                page.surface = None;
            }
        }
    }

    fn close(&mut self, pid: &str) {
        if let Some(page) = self.pages.remove(pid) {
            log::info!("browser-host: page {pid} closed");
            if let Some(s) = page.surface {
                self.respond(s.req, Ok(Value::Null));
            }
        }
    }

    /// Let Servo run, then pass on what the pages said and send new frames.
    pub fn run(&mut self) {
        self.servo.spin_event_loop();
        let mut crashed = Vec::new();
        let mut parts = Vec::new();
        for (pid, page) in &mut self.pages {
            for event in page.delegate.events.borrow_mut().drain(..) {
                parts.push((page.open, serde_json::to_value(event).expect("serializes")));
            }
            if let Some(reason) = page.delegate.crashed.borrow_mut().take() {
                log::warn!("browser-host: page {pid} crashed: {reason}");
                parts.push((page.open, json!({"event": "crashed", "reason": reason})));
                crashed.push((pid.clone(), page.open));
                continue;
            }
            if page.delegate.frame.take()
                && let Some(s) = &mut page.surface
            {
                s.want_frame = true;
            }
            if page.surface.as_ref().is_some_and(|s| s.want_frame && s.has_free_buffer())
                && let Some(frame) = paint(page)
            {
                let _ = self.out.send(Out::Part { id: frame.0, data: frame.1, fds: Vec::new() });
            }
        }
        for (id, data) in parts {
            let _ = self.out.send(Out::Part { id, data, fds: Vec::new() });
        }
        for (pid, open) in crashed {
            self.close(&pid);
            self.respond(open, Ok(Value::Null));
        }
    }
}

/// Lay `page` out at `w` by `h` pixels, `scale` of them to a CSS pixel, with new
/// buffers: the part announcing them, and their descriptors.
fn size(page: &mut Page, w: u32, h: u32, scale: f32) -> Result<(Value, Vec<std::os::fd::OwnedFd>), BusError> {
    let surface = page.surface.as_mut().expect("attached");
    let changed = (surface.width, surface.height, surface.scale) != (w, h, scale);
    let sized =
        surface.resize(w, h, scale).map_err(|e| error("unavailable", format!("no shared memory: {e}")))?;
    if changed {
        // The webview resizes its rendering context, and lays the page out again
        // only if that context's size changed: so not `page.rc.resize` first.
        page.view.resize(PhysicalSize::new(w, h));
        page.view.set_hidpi_scale_factor(Scale::new(scale));
    }
    Ok(sized)
}

/// Draw `page`'s latest frame into a free buffer of its surface: the attachment
/// to tell, and the part saying which buffer.
fn paint(page: &mut Page) -> Option<(u64, Value)> {
    let surface = page.surface.as_mut()?;
    page.rc.make_current().ok()?;
    page.view.paint();
    let size = page.rc.size2d().to_i32();
    let image = page.rc.read_to_image(DeviceIntRect::from_size(size));
    page.rc.present();
    let image = image?;
    match surface.put(image.width(), image.height(), image.as_raw()) {
        Ok(part) => Some((surface.req, part?)),
        Err(e) => {
            log::warn!("browser-host: couldn't write a frame: {e}");
            None
        }
    }
}

/// What a surface's input does to its page.
enum Steer {
    Event(InputEvent),
    Focus(bool),
}

fn input(event: SurfaceInput) -> Steer {
    let at = |x: f32, y: f32| WebViewPoint::Device(DevicePoint::new(x, y));
    Steer::Event(match event {
        SurfaceInput::Move { x, y } => InputEvent::MouseMove(MouseMoveEvent::new(at(x, y))),
        SurfaceInput::Button { x, y, button, pressed } => {
            let action = if pressed { MouseButtonAction::Down } else { MouseButtonAction::Up };
            let button = match button {
                wire::Button::Left => MouseButton::Primary,
                wire::Button::Middle => MouseButton::Auxiliary,
                wire::Button::Right => MouseButton::Secondary,
            };
            InputEvent::MouseButton(MouseButtonEvent::new(action, button, at(x, y)))
        }
        // Servo's positive deltas reveal what's above and to the left: the other
        // way to the web's.
        SurfaceInput::Wheel { x, y, dx, dy } => {
            let delta =
                WheelDelta { x: -f64::from(dx), y: -f64::from(dy), z: 0.0, mode: WheelMode::DeltaPixel };
            InputEvent::Wheel(WheelEvent::new(delta, at(x, y)))
        }
        SurfaceInput::Key { key, pressed, mods } => {
            let state = if pressed { KeyState::Down } else { KeyState::Up };
            let key = if key.chars().count() == 1 {
                Key::Character(key)
            } else {
                Key::Named(key.parse::<NamedKey>().unwrap_or(NamedKey::Unidentified))
            };
            let mut m = Modifiers::empty();
            m.set(Modifiers::SHIFT, mods.shift);
            m.set(Modifiers::CONTROL, mods.ctrl);
            m.set(Modifiers::ALT, mods.alt);
            m.set(Modifiers::META, mods.logo);
            InputEvent::Keyboard(KeyboardEvent::new_without_event(
                state,
                key,
                Code::Unidentified,
                Location::Standard,
                m,
                false,
                false,
            ))
        }
        SurfaceInput::Focus { on } => return Steer::Focus(on),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surface_input_becomes_servo_input() {
        let Steer::Event(InputEvent::Wheel(w)) =
            input(SurfaceInput::Wheel { x: 1.0, y: 2.0, dx: 0.0, dy: 30.0 })
        else {
            panic!()
        };
        assert_eq!(w.delta.y, -30.0, "down the page is Servo's negative y");
        let Steer::Event(InputEvent::Keyboard(k)) = input(SurfaceInput::Key {
            key: "ArrowLeft".into(),
            pressed: true,
            mods: wire::Mods { shift: true, ..wire::Mods::default() },
        }) else {
            panic!()
        };
        assert_eq!(k.event.key, Key::Named(NamedKey::ArrowLeft));
        assert!(k.event.modifiers.contains(Modifiers::SHIFT));
        let Steer::Event(InputEvent::Keyboard(k)) =
            input(SurfaceInput::Key { key: "é".into(), pressed: false, mods: wire::Mods::default() })
        else {
            panic!()
        };
        assert_eq!((k.event.key, k.event.state), (Key::Character("é".into()), KeyState::Up));
        assert!(matches!(input(SurfaceInput::Focus { on: true }), Steer::Focus(true)));
    }
}

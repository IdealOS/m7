//! The desktop: the scene, the window manager, the pointer and keyboard, and app
//! windows, driven by input from a backend and by requests from the bus.
//!
//! Both backends own one and feed it; it says what to send back to apps. Keeping
//! this free of I/O means the whole path from a bus request or a click to the
//! scene is tested without a display or a broker.

use protocol::bus::code;
use protocol::scene::method;
use protocol::{BusError, Commit, OpenWindowParams, Sender};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::apps::{Apps, Reply};
use crate::input::{Effect, Input, Seat};
use crate::scene::Scene;
use crate::wm::WindowManager;

/// A request (or a cancel) addressed to the compositor.
#[derive(Debug)]
pub enum Call {
    Request { id: u64, from: Option<Sender>, method: String, params: Value, stream: bool },
    Cancel { id: u64 },
}

/// What to send back over the bus.
#[derive(Debug, PartialEq)]
pub enum Out {
    /// A part of a streamed answer (a window event).
    Part { id: u64, data: Value },
    /// The answer to request `id`, ending it.
    Respond { id: u64, result: Result<Value, BusError> },
}

pub struct Desktop {
    pub scene: Scene,
    pub wm: WindowManager,
    pub seat: Seat,
    pub apps: Apps,
}

impl Desktop {
    /// An empty desktop for a `w` by `h` screen.
    pub fn new(w: f32, h: f32) -> Desktop {
        Desktop {
            scene: Scene::new(),
            wm: WindowManager::default(),
            seat: Seat::new(w, h),
            apps: Apps::new(),
        }
    }

    /// Apply one input. A press on something an app listens to goes to the app
    /// (and raises its window); everything else goes to the window manager.
    pub fn input(&mut self, input: Input) -> (Effect, Vec<Out>) {
        let (x, y) = self.seat.cursor();
        match input {
            Input::Button { pressed: true } => {
                if let Some(window) = self.wm.window_at(&self.scene, x, y)
                    && self.apps.press(&mut self.scene, window, x, y)
                {
                    self.scene.raise(window);
                    return (Effect::default(), Vec::new());
                }
            }
            Input::Button { pressed: false } if self.apps.pressing() => {
                let reply = self.apps.release(&mut self.scene, x, y);
                return (Effect::default(), reply.into_iter().map(part).collect());
            }
            _ => {}
        }
        (self.seat.handle(input, &mut self.scene, &mut self.wm), Vec::new())
    }

    /// Handle one request or cancel from the bus.
    pub fn call(&mut self, call: Call) -> Vec<Out> {
        let (id, from, method, params, stream) = match call {
            Call::Cancel { id } => {
                if self.apps.close_request(&mut self.scene, id) {
                    log::info!("apps: closed a window (request {id} ended)");
                }
                return Vec::new();
            }
            Call::Request { id, from, method, params, stream } => (id, from, method, params, stream),
        };
        let fail = |message: String| vec![Out::Respond { id, result: Err(bad_request(message)) }];
        // The broker stamps every request; a window belongs to whoever opened it.
        let Some(from) = from else { return fail("requests need a sender".into()) };
        match method.as_str() {
            method::OPEN_WINDOW if !stream => fail("open_window must be sent as a streamed request".into()),
            method::OPEN_WINDOW => match parse::<OpenWindowParams>(params) {
                Ok(p) => {
                    let screen = self.seat.bounds();
                    // One line per window; tools/smoke-checks.ts counts them.
                    log::info!("apps: opened a window for {from:?}: {:?}", p.title);
                    vec![part(self.apps.open(&mut self.scene, from, id, &p, screen))]
                }
                Err(e) => fail(e),
            },
            method::COMMIT => {
                let result = parse::<Commit>(params)
                    .and_then(|c| self.apps.commit(&mut self.scene, &from, c))
                    .map(|()| Value::Null)
                    .map_err(bad_request);
                vec![Out::Respond { id, result }]
            }
            other => fail(format!("the compositor has no method {other:?}")),
        }
    }
}

fn part(reply: Reply) -> Out {
    Out::Part { id: reply.request, data: serde_json::to_value(reply.event).expect("events serialize") }
}

fn parse<T: DeserializeOwned>(params: Value) -> Result<T, String> {
    serde_json::from_value(params).map_err(|e| format!("bad params: {e}"))
}

fn bad_request(message: String) -> BusError {
    BusError { code: code::BAD_REQUEST.into(), message }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::WindowEvent;
    use serde_json::json;

    fn app(conn: u64) -> Option<Sender> {
        Some(Sender::App { app: "org.example.counter".into(), conn })
    }

    fn request(id: u64, from: Option<Sender>, method: &str, params: Value, stream: bool) -> Call {
        Call::Request { id, from, method: method.into(), params, stream }
    }

    #[test]
    fn a_click_reaches_the_app_and_its_answer_reaches_the_screen() {
        let mut d = Desktop::new(1280.0, 800.0);
        let out = d.call(request(7, app(3), "open_window", json!({"title": "Counter"}), true));
        let [Out::Part { id: 7, data }] = &out[..] else { panic!("{out:?}") };
        let WindowEvent::Opened { window, .. } = serde_json::from_value(data.clone()).unwrap() else {
            panic!()
        };
        let ui = json!({"window": window, "ops": [
            {"op": "create", "id": 2, "type": "button", "props": {"x": 20, "y": 20, "w": 120, "h": 32, "label": "Add"}},
            {"op": "insert", "parent": 1, "child": 2, "index": 0},
            {"op": "listen", "id": 2, "events": ["click"]}
        ]});
        assert_eq!(
            d.call(request(8, app(3), "commit", ui, false)),
            [Out::Respond { id: 8, result: Ok(Value::Null) }]
        );

        // The window opens at (60, 60); its content starts below the title bar.
        let (x, y) = (60.0 + 40.0, 60.0 + 36.0 + 30.0);
        d.input(Input::MotionTo { x, y });
        let (_, out) = d.input(Input::Button { pressed: true });
        assert!(out.is_empty());
        assert!(!d.wm.is_dragging());
        let (_, out) = d.input(Input::Button { pressed: false });
        assert_eq!(out, [Out::Part { id: 7, data: json!({"kind": "event", "node": 2, "event": "click"}) }]);

        // Dragging the title bar still moves the window.
        d.input(Input::MotionTo { x: 100.0, y: 70.0 });
        d.input(Input::Button { pressed: true });
        assert!(d.wm.is_dragging());
        d.input(Input::Button { pressed: false });

        // Cancelling the request removes the window.
        let nodes = d.scene.len();
        assert!(d.call(Call::Cancel { id: 7 }).is_empty());
        assert!(d.scene.len() < nodes);
        assert!(d.apps.is_empty());
    }

    #[test]
    fn bad_requests_get_errors() {
        let mut d = Desktop::new(800.0, 600.0);
        let err = |out: Vec<Out>| match &out[..] {
            [Out::Respond { result: Err(e), .. }] => e.message.clone(),
            other => panic!("{other:?}"),
        };
        assert!(
            err(d.call(request(1, app(1), "open_window", json!({"title": "x"}), false))).contains("streamed")
        );
        assert!(err(d.call(request(2, app(1), "open_window", json!({}), true))).contains("bad params"));
        assert!(err(d.call(request(3, None, "open_window", json!({"title": "x"}), true))).contains("sender"));
        assert!(
            err(d.call(request(4, app(1), "commit", json!({"window": 5, "ops": []}), false)))
                .contains("no window")
        );
        assert!(err(d.call(request(5, app(1), "explode", Value::Null, false))).contains("no method"));
    }
}

//! Hosted backend: the compositor runs inside a normal window (macOS or a Linux
//! desktop) through winit. This is the day-to-day development backend. Apps on the
//! bus open windows in it; the bus thread wakes the event loop with a user event.

use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use crate::backend::Options;
use crate::bus::{self, Bridge};
use crate::demo;
use crate::desktop::Desktop;
use crate::input::{self, Input, Mods};
use crate::render::Renderer;
use crate::scene::Rgba;
use crate::stats::FrameTimer;

/// The bus thread queued requests for the frame loop.
struct Wake;

pub fn run(opts: Options) -> Result<(), Box<dyn std::error::Error>> {
    let event_loop = EventLoop::<Wake>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    let bridge = bus::connect(move || {
        let _ = proxy.send_event(Wake);
    });
    let mut desktop = Desktop::new(0.0, 0.0);
    desktop.scene.set_dark(opts.dark);
    let mut app = App {
        opts,
        gpu: None,
        desktop,
        bridge,
        timer: FrameTimer::default(),
        modifiers: ModifiersState::empty(),
        next_hud: Instant::now(),
        cursor: crate::cursor::Shape::Arrow,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

struct Gpu {
    instance: wgpu::Instance,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    renderer: Renderer,
    // Dropped last: the surface must not outlive its window.
    window: Arc<Window>,
}

struct App {
    opts: Options,
    gpu: Option<Gpu>,
    desktop: Desktop,
    bridge: Option<Bridge>,
    timer: FrameTimer,
    modifiers: ModifiersState,
    next_hud: Instant,
    /// The pointer image the window shows.
    cursor: crate::cursor::Shape,
}

impl App {
    fn logical_size(gpu: &Gpu) -> (f32, f32) {
        let scale = gpu.window.scale_factor() as f32;
        (gpu.config.width as f32 / scale, gpu.config.height as f32 / scale)
    }

    /// Feed one input to the desktop, send apps their events, and redraw if the
    /// scene changed.
    fn input(&mut self, input: Input) {
        let (_, outs) = self.desktop.input(input);
        if let Some(b) = &self.bridge {
            b.send(outs);
        }
        // The system cursor, in the shape the desktop wants (resize arrows at
        // window edges and dividers).
        let shape = self.desktop.cursor_shape();
        if shape != self.cursor
            && let Some(gpu) = &self.gpu
        {
            use crate::cursor::Shape;
            use winit::window::CursorIcon;
            gpu.window.set_cursor(match shape {
                Shape::Arrow => CursorIcon::Default,
                Shape::ResizeEw => CursorIcon::EwResize,
                Shape::ResizeNs => CursorIcon::NsResize,
                Shape::ResizeNwse => CursorIcon::NwseResize,
                Shape::ResizeNesw => CursorIcon::NeswResize,
            });
            self.cursor = shape;
        }
        if self.desktop.scene.is_dirty()
            && let Some(gpu) = &self.gpu
        {
            gpu.window.request_redraw();
        }
    }

    fn redraw(&mut self) {
        let Some(gpu) = &mut self.gpu else { return };
        if !self.desktop.scene.is_dirty() && !self.opts.continuous {
            return; // frame-level damage tracking: nothing changed, draw nothing
        }
        let frame = match gpu.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) => frame,
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                gpu.window.request_redraw();
                return;
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Suboptimal(_) => {
                gpu.surface.configure(&gpu.device, &gpu.config);
                gpu.window.request_redraw();
                return;
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                gpu.surface = gpu.instance.create_surface(gpu.window.clone()).expect("recreate surface");
                gpu.surface.configure(&gpu.device, &gpu.config);
                gpu.window.request_redraw();
                return;
            }
            wgpu::CurrentSurfaceTexture::Validation => panic!("surface validation error"),
        };

        let started = Instant::now();
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        let list = self.desktop.draw_list();
        let stats = gpu.renderer.render(
            &gpu.device,
            &gpu.queue,
            &mut encoder,
            &view,
            (gpu.config.width, gpu.config.height),
            gpu.window.scale_factor() as f32,
            &list,
            Rgba::parse(demo::BACKGROUND).expect("valid color"),
            // A window surface's buffers have no known age: redraw them whole.
            None,
        );
        gpu.queue.submit(Some(encoder.finish()));
        gpu.window.pre_present_notify();
        gpu.queue.present(frame);
        self.desktop.scene.clear_dirty();
        self.timer.record(started.elapsed(), stats.quads, stats.texts);

        if self.opts.continuous {
            gpu.window.request_redraw();
        }
    }
}

impl ApplicationHandler<Wake> for App {
    fn user_event(&mut self, _: &ActiveEventLoop, _: Wake) {
        let Some(bridge) = &self.bridge else { return };
        for call in bridge.calls() {
            bridge.send(self.desktop.call(call));
        }
        if self.desktop.scene.is_dirty()
            && let Some(gpu) = &self.gpu
        {
            gpu.window.request_redraw();
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gpu.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("idealos — compositor (hosted)")
            .with_inner_size(LogicalSize::new(self.opts.width as f64, self.opts.height as f64));
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        let gpu = pollster::block_on(init_gpu(window, event_loop));
        let (w, h) = App::logical_size(&gpu);
        log::info!(
            "hosted: {}x{} logical at scale {}, format {:?}",
            w,
            h,
            gpu.window.scale_factor(),
            gpu.config.format
        );
        self.desktop.scene.apply_all(demo::build(w, h, self.opts.windows));
        self.desktop.set_screen(w, h);
        gpu.window.request_redraw();
        self.gpu = Some(gpu);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                let Some(gpu) = &mut self.gpu else { return };
                gpu.config.width = size.width.max(1);
                gpu.config.height = size.height.max(1);
                gpu.surface.configure(&gpu.device, &gpu.config);
                let (w, h) = App::logical_size(gpu);
                self.desktop.scene.apply_all(demo::resize(w, h));
                self.desktop.set_screen(w, h);
                gpu.window.request_redraw();
            }
            WindowEvent::CursorMoved { position, .. } => {
                let Some(gpu) = &self.gpu else { return };
                let scale = gpu.window.scale_factor();
                self.input(Input::MotionTo {
                    x: (position.x / scale) as f32,
                    y: (position.y / scale) as f32,
                });
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => {
                self.input(Input::Button { pressed: state == ElementState::Pressed });
            }
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                // macOS keeps Cmd+Tab for itself, so the window switcher is Option+Tab here.
                let m = self.modifiers;
                let mods = Mods {
                    ctrl: m.control_key(),
                    alt: m.alt_key(),
                    shift: m.shift_key(),
                    logo: m.super_key(),
                };
                if let Some(key) = winit_key(&event.logical_key) {
                    self.input(Input::Key { key, mods });
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let Some(gpu) = &self.gpu else { return };
                // Our convention: positive dy moves content up. winit's is the reverse.
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (-x * 48.0, -y * 48.0),
                    MouseScrollDelta::PixelDelta(p) => {
                        let scale = gpu.window.scale_factor();
                        ((-p.x / scale) as f32, (-p.y / scale) as f32)
                    }
                };
                self.input(Input::Scroll { dx, dy });
            }
            WindowEvent::RedrawRequested => self.redraw(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Refresh the frame-time readout once a second; this is the only periodic
        // redraw while the desktop is otherwise idle.
        let now = Instant::now();
        if now >= self.next_hud {
            let summary = self.timer.summary();
            if self.opts.continuous {
                log::info!("{summary}");
            }
            self.desktop.scene.apply(demo::hud(&summary));
            self.timer.reset_window();
            self.next_hud = now + Duration::from_secs(1);
            if let Some(gpu) = &self.gpu {
                gpu.window.request_redraw();
            }
        }
        // Coasting scroll views, the window switcher's timeout, the clock.
        let animating = self.desktop.tick(now);
        if (animating || self.desktop.scene.is_dirty())
            && let Some(gpu) = &self.gpu
        {
            gpu.window.request_redraw();
        }
        if !self.opts.continuous {
            event_loop
                .set_control_flow(ControlFlow::WaitUntil(self.next_hud.min(self.desktop.next_wake(now))));
        }
    }
}

/// winit's key as ours, if it's one the compositor uses.
fn winit_key(key: &Key) -> Option<input::Key> {
    use input::Key as K;
    Some(match key {
        Key::Named(NamedKey::Tab) => K::Tab,
        Key::Named(NamedKey::Enter) => K::Enter,
        Key::Named(NamedKey::Escape) => K::Escape,
        Key::Named(NamedKey::Backspace) => K::Backspace,
        Key::Named(NamedKey::Delete) => K::Delete,
        Key::Named(NamedKey::ArrowLeft) => K::Left,
        Key::Named(NamedKey::ArrowRight) => K::Right,
        Key::Named(NamedKey::ArrowUp) => K::Up,
        Key::Named(NamedKey::ArrowDown) => K::Down,
        Key::Named(NamedKey::Home) => K::Home,
        Key::Named(NamedKey::End) => K::End,
        Key::Named(NamedKey::PageUp) => K::PageUp,
        Key::Named(NamedKey::PageDown) => K::PageDown,
        Key::Named(NamedKey::Space) => K::Char(' '),
        Key::Character(s) => K::Char(s.chars().next()?),
        _ => return None,
    })
}

async fn init_gpu(window: Arc<Window>, event_loop: &ActiveEventLoop) -> Gpu {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle_from_env(Box::new(
        event_loop.owned_display_handle(),
    )));
    let surface = instance.create_surface(window.clone()).expect("create surface");
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(&surface),
            ..Default::default()
        })
        .await
        .expect("no compatible GPU adapter");
    log::info!("adapter: {:?}", adapter.get_info());
    let (device, queue) =
        adapter.request_device(&wgpu::DeviceDescriptor::default()).await.expect("request device");

    let caps = surface.get_capabilities(&adapter);
    let format = caps.formats.iter().copied().find(|f| f.is_srgb()).unwrap_or(caps.formats[0]);
    let size = window.inner_size();
    let config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format,
        width: size.width.max(1),
        height: size.height.max(1),
        present_mode: wgpu::PresentMode::Fifo,
        alpha_mode: caps.alpha_modes[0],
        view_formats: vec![],
        desired_maximum_frame_latency: 2,
        color_space: wgpu::SurfaceColorSpace::Auto,
    };
    surface.configure(&device, &config);
    let renderer = Renderer::new(&device, &queue, format);
    Gpu { instance, device, queue, surface, config, renderer, window }
}

//! Hosted backend: the compositor runs inside a normal window (macOS or a Linux
//! desktop) through winit. This is the day-to-day development backend.

use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

use crate::backend::Options;
use crate::demo;
use crate::render::Renderer;
use crate::scene::{Rgba, Scene};
use crate::stats::FrameTimer;
use crate::wm::WindowManager;

pub fn run(opts: Options) -> Result<(), Box<dyn std::error::Error>> {
    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App {
        opts,
        gpu: None,
        scene: Scene::new(),
        wm: WindowManager::default(),
        timer: FrameTimer::default(),
        cursor: (0.0, 0.0),
        next_hud: Instant::now(),
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
    scene: Scene,
    wm: WindowManager,
    timer: FrameTimer,
    cursor: (f32, f32),
    next_hud: Instant,
}

impl App {
    fn logical_size(gpu: &Gpu) -> (f32, f32) {
        let scale = gpu.window.scale_factor() as f32;
        (gpu.config.width as f32 / scale, gpu.config.height as f32 / scale)
    }

    fn redraw(&mut self) {
        let Some(gpu) = &mut self.gpu else { return };
        if !self.scene.is_dirty() && !self.opts.continuous {
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
        let list = self.scene.draw_list();
        let stats = gpu.renderer.render(
            &gpu.device,
            &gpu.queue,
            &mut encoder,
            &view,
            (gpu.config.width, gpu.config.height),
            gpu.window.scale_factor() as f32,
            &list,
            Rgba::parse(demo::BACKGROUND).expect("valid color"),
        );
        gpu.queue.submit(Some(encoder.finish()));
        gpu.window.pre_present_notify();
        gpu.queue.present(frame);
        self.scene.clear_dirty();
        self.timer.record(started.elapsed(), stats.quads, stats.texts);

        if self.opts.continuous {
            gpu.window.request_redraw();
        }
    }
}

impl ApplicationHandler for App {
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
        self.scene.apply_all(demo::build(w, h, self.opts.windows));
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
                self.scene.apply_all(demo::resize(w, h));
                gpu.window.request_redraw();
            }
            WindowEvent::CursorMoved { position, .. } => {
                let Some(gpu) = &self.gpu else { return };
                let scale = gpu.window.scale_factor();
                self.cursor = ((position.x / scale) as f32, (position.y / scale) as f32);
                if self.wm.is_dragging() {
                    self.wm.pointer_move(&mut self.scene, self.cursor.0, self.cursor.1);
                    gpu.window.request_redraw();
                }
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => {
                match state {
                    ElementState::Pressed => {
                        self.wm.pointer_down(&mut self.scene, self.cursor.0, self.cursor.1)
                    }
                    ElementState::Released => self.wm.pointer_up(),
                }
                if let Some(gpu) = &self.gpu {
                    gpu.window.request_redraw();
                }
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
            self.scene.apply(demo::hud(&summary));
            self.timer.reset_window();
            self.next_hud = now + Duration::from_secs(1);
            if let Some(gpu) = &self.gpu {
                gpu.window.request_redraw();
            }
        }
        if !self.opts.continuous {
            event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_hud));
        }
    }
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

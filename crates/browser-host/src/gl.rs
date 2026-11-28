/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 *
 * Adapted from Servo's `SoftwareRenderingContext` (components/shared/paint/
 * rendering_context.rs, servo-paint-api 0.8.0), so this file alone stays under
 * the MPL; the rest of browser-host is MIT or Apache-2.0. */

//! A rendering context on the GPU, for drawing pages with no window.
//!
//! Servo's own offscreen context, `SoftwareRenderingContext`, asks Mesa for its
//! CPU renderer (it sets `LIBGL_ALWAYS_SOFTWARE`), which the Pi image doesn't
//! have and wouldn't want: the Pi 4's GPU (V3D) does GLES 3.1. This is the same
//! context on the GPU when there is one, through Mesa's surfaceless EGL (a render
//! node, no display), falling back to the CPU renderer (QEMU without virgl, say).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use dpi::PhysicalSize;
use euclid::default::Size2D;
use gleam::gl::{self, Gl};
use servo::{DeviceIntRect, RenderingContext, RgbaImage};
use surfman::chains::{PreserveBuffer, SwapChain};
// On Linux, surfman's surfaceless-Mesa backend: EGL on a render node, with no
// display. (Its default there is Wayland, which loads libwayland-client at run
// time and panics without it; the image has no Wayland.) Elsewhere, the default.
#[cfg(target_os = "linux")]
use surfman::mesa_surfaceless::{
    connection::Connection,
    context::Context,
    device::{Adapter, Device},
};
#[cfg(not(target_os = "linux"))]
use surfman::{Adapter, Connection, Context, Device};
use surfman::{
    ContextAttributeFlags, ContextAttributes, Error, GLApi, GLVersion, SurfaceAccess, SurfaceType,
};

pub struct GpuContext {
    size: Cell<PhysicalSize<u32>>,
    gleam: Rc<dyn Gl>,
    glow: Arc<glow::Context>,
    device: RefCell<Device>,
    context: RefCell<Context>,
    swap_chain: SwapChain<Device>,
}

impl GpuContext {
    /// A `size` context on the GPU, or on the CPU if the GPU won't make one.
    pub fn new(size: PhysicalSize<u32>) -> Result<GpuContext, Error> {
        if size.width == 0 || size.height == 0 {
            return Err(Error::Failed);
        }
        let connection = Connection::new()?;
        // The low-power adapter is the plain hardware one (the "high-performance"
        // one sets DRI_PRIME, for laptops with two GPUs).
        match GpuContext::with(&connection, &connection.create_low_power_adapter()?, size) {
            Ok(c) => Ok(c),
            Err(e) => {
                log::warn!("browser-host: no GPU context ({e:?}); drawing pages on the CPU");
                GpuContext::with(&connection, &connection.create_software_adapter()?, size)
            }
        }
    }

    fn with(
        connection: &Connection,
        adapter: &Adapter,
        size: PhysicalSize<u32>,
    ) -> Result<GpuContext, Error> {
        let device = connection.create_device(adapter)?;
        let api = connection.gl_api();
        let version = match api {
            GLApi::GLES => GLVersion { major: 3, minor: 0 },
            GLApi::GL => GLVersion { major: 3, minor: 2 },
        };
        let flags =
            ContextAttributeFlags::ALPHA | ContextAttributeFlags::DEPTH | ContextAttributeFlags::STENCIL;
        let descriptor = device.create_context_descriptor(&ContextAttributes { flags, version })?;
        let mut context = device.create_context(&descriptor, None)?;
        let ready = (|| {
            device.make_context_current(&context)?;
            let surface = device.create_surface(
                &context,
                SurfaceAccess::GPUOnly,
                SurfaceType::Generic { size: Size2D::new(size.width as i32, size.height as i32) },
            )?;
            device.bind_surface_to_context(&mut context, surface).map_err(|(e, mut surface)| {
                let _ = device.destroy_surface(&mut context, &mut surface);
                e
            })?;
            device.make_context_current(&context)?;
            SwapChain::create_attached(&device, &mut context, SurfaceAccess::GPUOnly)
        })();
        let swap_chain = match ready {
            Ok(s) => s,
            Err(e) => {
                let _ = device.destroy_context(&mut context);
                return Err(e);
            }
        };
        // Safety: the context is current, and the loaders only look up GL entry points.
        let gleam = unsafe {
            match api {
                GLApi::GL => gl::GlFns::load_with(|f| device.get_proc_address(&context, f)),
                GLApi::GLES => gl::GlesFns::load_with(|f| device.get_proc_address(&context, f)),
            }
        };
        let glow = unsafe { glow::Context::from_loader_function(|f| device.get_proc_address(&context, f)) };
        log::info!("browser-host: drawing pages with {}", gleam.get_string(gl::RENDERER));
        Ok(GpuContext {
            size: Cell::new(size),
            gleam,
            glow: Arc::new(glow),
            device: RefCell::new(device),
            context: RefCell::new(context),
            swap_chain,
        })
    }

    /// The framebuffer Servo draws into: the bound surface's.
    fn framebuffer(&self) -> u32 {
        let device = self.device.borrow();
        let context = self.context.borrow();
        device
            .context_surface_info(&context)
            .ok()
            .flatten()
            .and_then(|info| info.framebuffer_object)
            .map_or(0, |fb| fb.0.get())
    }
}

impl Drop for GpuContext {
    fn drop(&mut self) {
        let device = &mut self.device.borrow_mut();
        let context = &mut self.context.borrow_mut();
        let _ = self.swap_chain.destroy(device, context);
        let _ = device.destroy_context(context);
    }
}

impl RenderingContext for GpuContext {
    fn prepare_for_rendering(&self) {
        self.gleam.bind_framebuffer(gl::FRAMEBUFFER, self.framebuffer());
    }

    fn read_to_image(&self, rect: DeviceIntRect) -> Option<RgbaImage> {
        self.gleam.bind_framebuffer(gl::FRAMEBUFFER, self.framebuffer());
        self.gleam.bind_vertex_array(0);
        let (w, h) = (rect.width(), rect.height());
        let mut pixels = self.gleam.read_pixels(rect.min.x, rect.min.y, w, h, gl::RGBA, gl::UNSIGNED_BYTE);
        // GL's rows go bottom up.
        let (stride, h) = (w as usize * 4, h as usize);
        for y in 0..h / 2 {
            let (top, bottom) = pixels.split_at_mut((h - y - 1) * stride);
            top[y * stride..(y + 1) * stride].swap_with_slice(&mut bottom[..stride]);
        }
        RgbaImage::from_raw(w as u32, h as u32, pixels)
    }

    fn size(&self) -> PhysicalSize<u32> {
        self.size.get()
    }

    fn resize(&self, size: PhysicalSize<u32>) {
        if size.width == 0 || size.height == 0 || self.size.get() == size {
            return;
        }
        self.size.set(size);
        let device = &mut self.device.borrow_mut();
        let context = &mut self.context.borrow_mut();
        let _ = self.swap_chain.resize(device, context, Size2D::new(size.width as i32, size.height as i32));
    }

    fn present(&self) {
        let device = &mut self.device.borrow_mut();
        let context = &mut self.context.borrow_mut();
        let _ = self.swap_chain.swap_buffers(device, context, PreserveBuffer::No);
    }

    fn make_current(&self) -> Result<(), Error> {
        self.device.borrow().make_context_current(&self.context.borrow())
    }

    fn gleam_gl_api(&self) -> Rc<dyn Gl> {
        self.gleam.clone()
    }

    fn glow_gl_api(&self) -> Arc<glow::Context> {
        self.glow.clone()
    }

    /// For WebGL, which makes its own contexts on the same device. Servo wants
    /// surfman's default connection type, which on Linux isn't the surfaceless
    /// one this context uses: so no WebGL there yet.
    #[cfg(not(target_os = "linux"))]
    fn connection(&self) -> Option<Connection> {
        Some(self.device.borrow().connection())
    }
}

//! KMS backend: full-screen output straight to DRM/KMS with libinput for input.
//!
//! Not implemented yet. It depends on two spikes from the implementation plan:
//!
//! - **Spike A (QEMU):** lavapipe renders into a wgpu texture, which is copied
//!   into a KMS dumb buffer and page-flipped. virgl GLES is the faster variant.
//! - **Spike B (Pi 4):** V3DV renders into a texture exported as a dmabuf, which
//!   is imported as a KMS framebuffer with no copy.
//!
//! The intended shape, so the rest of the compositor does not change:
//!
//! 1. Open `/dev/dri/card0`, pick the connected connector and its preferred mode.
//! 2. Create two scanout buffers (dmabuf-backed, or dumb buffers on lavapipe).
//! 3. Each frame: `Renderer::render` into the back buffer's texture view, then
//!    `drmModePageFlip` and wait for the flip event (that event is vsync).
//! 4. libinput (through `input` and `xkbcommon`) feeds the same
//!    [`crate::wm::WindowManager`] calls the hosted backend makes.

use crate::backend::Options;

pub fn run(_opts: Options) -> Result<(), Box<dyn std::error::Error>> {
    Err("the KMS backend is not implemented yet; see Spike A and Spike B in docs/implementation-plan.md"
        .into())
}

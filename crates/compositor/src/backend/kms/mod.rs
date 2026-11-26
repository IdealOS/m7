//! KMS backend: full-screen output straight to DRM/KMS, for the device images.
//!
//! Frames reach the display one of two ways (`--scanout`, see
//! [`crate::backend::Scanout`]), into one of three KMS dumb buffers that are
//! page-flipped on vblank:
//!
//! - **Direct (Spike B):** each dumb buffer is exported as a dma-buf and imported into
//!   the GPU as a linear texture ([`crate::headless::Offscreen::import_dmabuf`]), and
//!   the frame is drawn straight into it. No readback, no copy. On a Pi 4 (V3DV) at
//!   3440x1440 a frame went from 125 ms to 57 ms. The default wherever the GPU's driver
//!   can import dma-bufs.
//! - **Copy (Spike A):** wgpu renders into an offscreen texture, the pixels are read
//!   back and copied into the dumb buffer. Works with any Vulkan driver, lavapipe
//!   included (QEMU, the Pi 3), whose frames are in ordinary memory anyway.
//!
//! It can also render fewer rows than the display mode and have the display's primary
//! plane scale the frame up, in the display hardware (`--render-height`; see
//! [`crate::backend::RenderHeight`]). That's off by default: text comes out fuzzy.
//!
//! Input comes from libinput ([`input`]), with keymaps from xkbcommon, and feeds the
//! same [`crate::input::Seat`] as the hosted backend. The pointer is drawn on the
//! display's cursor plane, so moving it costs no redraw. While it runs, the text
//! console's keyboard is off and the console doesn't draw ([`console`]), and SIGTERM
//! ends the frame loop normally so both come back ([`signals`]).
//!
//! Still to come (milestone M1):
//!
//! - **Region damage:** today any change redraws the whole frame, which on the Pi 4 is
//!   now most of a frame's cost (drawing 5 million pixels at 3440x1440).
//! - **Fences instead of waiting:** a direct frame blocks until the GPU finishes before
//!   the flip; a fence the display waits on would let the next frame start sooner.
//!
//! [`display`] is shared with `spikes/kms-probe`, which measures the same path.

#[cfg(target_os = "linux")]
mod console;
#[cfg(target_os = "linux")]
pub mod display;
#[cfg(target_os = "linux")]
pub mod input;
#[cfg(target_os = "linux")]
mod run;
#[cfg(target_os = "linux")]
mod signals;

#[cfg(target_os = "linux")]
pub use run::run;

#[cfg(not(target_os = "linux"))]
pub fn run(_opts: crate::backend::Options) -> Result<(), Box<dyn std::error::Error>> {
    Err("the KMS backend drives DRM/KMS and only runs on Linux".into())
}

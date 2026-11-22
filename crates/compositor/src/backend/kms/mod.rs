//! KMS backend: full-screen output straight to DRM/KMS, for the device images.
//!
//! This is the copy path that Spike A proved in QEMU: wgpu renders the scene into an
//! offscreen texture ([`crate::headless::Offscreen`]), the pixels are read back and
//! copied into one of three KMS dumb buffers, and that buffer is page-flipped on
//! vblank. It works with any Vulkan driver, lavapipe included.
//!
//! Input comes from libinput ([`input`]), with keymaps from xkbcommon, and feeds the
//! same [`crate::input::Seat`] as the hosted backend. The pointer is drawn on the
//! display's cursor plane, so moving it costs no redraw. While it runs, the text
//! console's keyboard is off and the console doesn't draw ([`console`]), and SIGTERM
//! ends the frame loop normally so both come back ([`signals`]).
//!
//! Still to come (milestone M1):
//!
//! - **Zero-copy scanout on the Pi (Spike B):** V3DV renders into a texture exported as
//!   a dmabuf and imported as the KMS framebuffer, with no readback or copy.
//! - **Region damage:** today any change redraws and copies the whole frame.
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

//! KMS backend: full-screen output straight to DRM/KMS, for the device images.
//!
//! This is the copy path that Spike A proved in QEMU: wgpu renders the scene into an
//! offscreen texture ([`crate::headless::Offscreen`]), the pixels are read back and
//! copied into one of three KMS dumb buffers, and that buffer is page-flipped on
//! vblank. It works with any Vulkan driver, lavapipe included.
//!
//! Still to come (milestone M1):
//!
//! - **Zero-copy scanout on the Pi (Spike B):** V3DV renders into a texture exported as
//!   a dmabuf and imported as the KMS framebuffer, with no readback or copy.
//! - **Input:** libinput (through `input` and `xkbcommon`) feeding the same
//!   [`crate::wm::WindowManager`] calls the hosted backend makes.
//! - **Region damage:** today any change redraws and copies the whole frame.
//!
//! [`display`] is shared with `spikes/kms-probe`, which measures the same path.

#[cfg(target_os = "linux")]
pub mod display;
#[cfg(target_os = "linux")]
mod run;

#[cfg(target_os = "linux")]
pub use run::run;

#[cfg(not(target_os = "linux"))]
pub fn run(_opts: crate::backend::Options) -> Result<(), Box<dyn std::error::Error>> {
    Err("the KMS backend drives DRM/KMS and only runs on Linux".into())
}

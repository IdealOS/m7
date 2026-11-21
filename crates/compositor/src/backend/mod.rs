//! Display backends. Each owns its event loop and feeds the shared scene,
//! window manager and renderer.

#[cfg(feature = "hosted")]
pub mod hosted;
#[cfg(feature = "kms")]
pub mod kms;

#[derive(Debug, Clone)]
pub struct Options {
    /// Logical size of the hosted window.
    pub width: u32,
    pub height: u32,
    /// Windows in the demo scene.
    pub windows: usize,
    /// Redraw every vsync even when nothing changed, to measure frame time.
    pub continuous: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options { width: 1440, height: 900, windows: 20, continuous: false }
    }
}

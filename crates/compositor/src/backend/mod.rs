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
    /// DRM device for the KMS backend; `None` picks the first with a connected display.
    pub card: Option<String>,
    /// How many rows the KMS backend renders.
    pub render_height: RenderHeight,
    /// How finished frames reach the KMS backend's scanout buffers.
    pub scanout: Scanout,
    /// Start in the dark theme. The `appearance.theme` setting, once read, decides.
    pub dark: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            width: 1440,
            height: 900,
            windows: 20,
            continuous: false,
            card: None,
            render_height: RenderHeight::Native,
            scanout: Scanout::Auto,
            dark: false,
        }
    }
}

/// How finished frames reach the display in the KMS backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scanout {
    /// Direct where the GPU can import the display's buffers, copy otherwise.
    Auto,
    /// Render offscreen, read the pixels back and copy them into the display's buffer.
    /// Works everywhere, lavapipe included; on a Pi 4 at 3440x1440 it costs ~70 ms a
    /// frame on top of drawing.
    Copy,
    /// The GPU renders straight into the display's buffers, shared as dma-bufs: no
    /// readback or copy. Fails if the driver can't import them.
    Direct,
}

impl Scanout {
    /// `auto`, `copy` or `direct`.
    pub fn parse(s: &str) -> Option<Scanout> {
        match s {
            "auto" => Some(Scanout::Auto),
            "copy" => Some(Scanout::Copy),
            "direct" => Some(Scanout::Direct),
            _ => None,
        }
    }
}

/// How many rows the KMS backend renders. A frame smaller than the display mode is
/// scaled up to fill it by the display hardware, for free, but text comes out fuzzy:
/// 720 rows on a 1080p display spreads each pixel over one and a half. The default is
/// [`RenderHeight::Native`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderHeight {
    /// At most [`CPU_RENDER_HEIGHT`] rows when rendering on the CPU (lavapipe, as on a
    /// Pi 3), whose cost grows with the pixel count; the display's own size on a GPU.
    Auto,
    /// The display's own size.
    Native,
    /// At most this many rows.
    AtMost(u32),
}

/// 720p: a 1080p display then renders 44% of its pixels. On a Pi 3B+ that made text
/// fuzzy without a visible speed-up, so it is opt-in.
pub const CPU_RENDER_HEIGHT: u32 = 720;

impl RenderHeight {
    /// `auto`, `native`, or a number of rows.
    pub fn parse(s: &str) -> Option<RenderHeight> {
        match s {
            "auto" => Some(RenderHeight::Auto),
            "native" => Some(RenderHeight::Native),
            n => n.parse().ok().filter(|&n| n > 0).map(RenderHeight::AtMost),
        }
    }

    /// The size to render for a display `mode` (width, height), with the shape of the
    /// mode. `cpu` says the renderer runs on the CPU.
    pub fn size(self, mode: (u32, u32), cpu: bool) -> (u32, u32) {
        let limit = match self {
            RenderHeight::Auto if cpu => CPU_RENDER_HEIGHT,
            RenderHeight::Auto | RenderHeight::Native => return mode,
            RenderHeight::AtMost(rows) => rows,
        };
        let (w, h) = mode;
        if h <= limit {
            return mode;
        }
        // Round to the nearest column, so 1920x1080 at 720 rows is exactly 1280x720.
        let width = (u64::from(w) * u64::from(limit) + u64::from(h) / 2) / u64::from(h);
        (width.max(1) as u32, limit)
    }
}

#[cfg(test)]
mod tests {
    use super::RenderHeight::{self, *};

    #[test]
    fn parses_scanout_modes() {
        use super::Scanout;
        assert_eq!(Scanout::parse("auto"), Some(Scanout::Auto));
        assert_eq!(Scanout::parse("copy"), Some(Scanout::Copy));
        assert_eq!(Scanout::parse("direct"), Some(Scanout::Direct));
        assert_eq!(Scanout::parse("zero-copy"), None);
        assert_eq!(super::Options::default().scanout, Scanout::Auto);
    }

    #[test]
    fn parses_render_heights() {
        assert_eq!(RenderHeight::parse("auto"), Some(Auto));
        assert_eq!(RenderHeight::parse("native"), Some(Native));
        assert_eq!(RenderHeight::parse("720"), Some(AtMost(720)));
        assert_eq!(RenderHeight::parse("0"), None);
        assert_eq!(RenderHeight::parse("720p"), None);
    }

    #[test]
    fn renders_at_the_display_size_by_default() {
        let default = super::Options::default().render_height;
        assert_eq!(default, Native);
        assert_eq!(default.size((1920, 1080), true), (1920, 1080));
    }

    #[test]
    fn auto_renders_720p_only_on_the_cpu() {
        assert_eq!(Auto.size((1920, 1080), true), (1280, 720));
        assert_eq!(Auto.size((1920, 1080), false), (1920, 1080));
        assert_eq!(Native.size((1920, 1080), true), (1920, 1080));
    }

    #[test]
    fn keeps_the_mode_shape_and_never_grows() {
        assert_eq!(AtMost(720).size((3840, 2160), false), (1280, 720));
        assert_eq!(AtMost(720).size((1280, 1024), false), (900, 720));
        assert_eq!(AtMost(720).size((1280, 800), false), (1152, 720));
        // Already small enough: render at the mode, unscaled.
        assert_eq!(AtMost(720).size((1024, 600), false), (1024, 600));
        assert_eq!(AtMost(720).size((1280, 720), false), (1280, 720));
    }
}

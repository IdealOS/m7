//! Theme tokens: the colors, spacing, type sizes and radii the compositor draws
//! with, in one place, ready for theming later.
//!
//! Apps can use a token anywhere a color goes by naming it with a `$`
//! (`"color": "$accent"`), so their UI follows the theme.

/// Colors, as `#rrggbb` or `#rrggbbaa`.
pub mod color {
    pub const DESKTOP: &str = "#dfe5ec";
    pub const SURFACE: &str = "#ffffff";
    pub const SURFACE_ALT: &str = "#f3f5f8";
    pub const BORDER: &str = "#b8c1cc";
    pub const TEXT: &str = "#1f2937";
    pub const MUTED: &str = "#6b7280";
    pub const ACCENT: &str = "#3b82f6";
    pub const ACCENT_TEXT: &str = "#ffffff";
    pub const CONTROL: &str = "#eef1f6";
    pub const CONTROL_PRESSED: &str = "#c9d2df";
    pub const CONTROL_BORDER: &str = "#aeb8c6";
    pub const FOCUS: &str = "#3b82f6";
    pub const SELECTION: &str = "#bfdbfe";
    pub const DANGER: &str = "#ef4444";
    /// Behind a dialog, over the rest of its window.
    pub const BACKDROP: &str = "#0f172a59";
    pub const BAR: &str = "#ffffffe6";
    /// Over the whole screen, behind a permission prompt.
    pub const SCRIM: &str = "#0f172ab3";
}

/// Spacing steps, in logical pixels.
pub mod space {
    pub const XS: f32 = 4.0;
    pub const S: f32 = 8.0;
    pub const M: f32 = 12.0;
    pub const L: f32 = 16.0;
    pub const XL: f32 = 24.0;
}

/// Type sizes, in logical pixels.
pub mod size {
    pub const SMALL: f32 = 12.0;
    pub const BODY: f32 = 14.0;
    pub const TITLE: f32 = 18.0;
    pub const HEADING: f32 = 22.0;
    /// The range of text sizes an app may ask for. Glyphs are rasterized at their
    /// size, so an unbounded one would let an app ask for a gigabyte glyph.
    pub const MIN: f32 = 4.0;
    pub const MAX: f32 = 200.0;

    /// A text size from an app, kept within [`MIN`, `MAX`].
    pub fn clamp(size: f32) -> f32 {
        size.clamp(MIN, MAX)
    }
}

/// Corner radii.
pub mod radius {
    pub const S: f32 = 4.0;
    pub const M: f32 = 6.0;
    pub const L: f32 = 10.0;
}

/// The color a `$name` token stands for.
pub fn token(name: &str) -> Option<&'static str> {
    Some(match name {
        "desktop" => color::DESKTOP,
        "surface" => color::SURFACE,
        "surface-alt" => color::SURFACE_ALT,
        "border" => color::BORDER,
        "text" => color::TEXT,
        "muted" => color::MUTED,
        "accent" => color::ACCENT,
        "accent-text" => color::ACCENT_TEXT,
        "control" => color::CONTROL,
        "selection" => color::SELECTION,
        "danger" => color::DANGER,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::Rgba;

    #[test]
    fn every_token_is_a_valid_color() {
        for name in [
            "desktop",
            "surface",
            "surface-alt",
            "border",
            "text",
            "muted",
            "accent",
            "accent-text",
            "control",
            "selection",
            "danger",
        ] {
            assert!(Rgba::parse(token(name).unwrap()).is_some(), "{name}");
        }
        assert_eq!(token("nope"), None);
        assert!(Rgba::parse(color::BACKDROP).is_some());
    }
}

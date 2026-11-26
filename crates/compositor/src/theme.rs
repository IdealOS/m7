//! Theme tokens the style sheets don't cover: spacing, type sizes and radii.
//! Colours and fonts come from the style sheets ([`crate::style`]); a colour prop can
//! name one of their variables as `$accent` or `var(--accent)`.

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

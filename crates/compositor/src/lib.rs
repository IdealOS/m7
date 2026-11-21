//! Scene-graph compositor.
//!
//! - [`scene`]: the retained node tree apps mutate with protocol ops.
//! - [`render`]: wgpu renderer for SDF rects and shaped text.
//! - [`wm`]: window stacking and dragging, handled without app round trips.
//! - [`backend`]: hosted (winit) and KMS display backends.
//! - [`headless`]: offscreen rendering for screenshots and golden-image tests.

pub mod backend;
pub mod demo;
pub mod headless;
pub mod render;
pub mod scene;
pub mod stats;
pub mod wm;

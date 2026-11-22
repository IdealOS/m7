//! Scene-graph compositor.
//!
//! - [`scene`]: the retained node tree apps mutate with protocol ops.
//! - [`render`]: wgpu renderer for SDF rects and shaped text.
//! - [`wm`]: window stacking and dragging, handled without app round trips.
//! - [`input`]: the pointer and keyboard both backends feed into the window manager.
//! - [`backend`]: hosted (winit) and KMS display backends.
//! - [`headless`]: offscreen rendering for screenshots and golden-image tests.

pub mod backend;
pub mod cursor;
pub mod demo;
pub mod headless;
pub mod input;
pub mod render;
pub mod scene;
pub mod stats;
pub mod wm;

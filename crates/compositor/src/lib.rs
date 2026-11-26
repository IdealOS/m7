//! Scene-graph compositor.
//!
//! - [`scene`]: the retained node tree apps mutate with protocol ops.
//! - [`apps`]: app windows: frames, per-window node ids, commits and clicks.
//! - [`render`]: wgpu renderer for SDF rects and shaped text.
//! - [`damage`]: what changed between frames, to redraw only that.
//! - [`fonts`]: the built-in UI font (Inter).
//! - [`wm`]: window stacking and dragging, handled without app round trips.
//! - [`input`]: the pointer and keyboard both backends feed into the window manager.
//! - [`backend`]: hosted (winit) and KMS display backends.
//! - [`headless`]: offscreen rendering for screenshots and golden-image tests.

pub mod apps;
pub mod backend;
pub mod bus;
pub mod cursor;
pub mod damage;
pub mod demo;
pub mod desktop;
pub mod draw;
pub mod fonts;
pub mod headless;
pub mod image;
pub mod input;
pub mod layout;
pub mod prompt;
pub mod render;
pub mod scene;
pub mod shell;
pub mod stats;
pub mod terminal;
pub mod textedit;
pub mod theme;
pub mod wm;

//! Input that both backends share: one pointer and one keyboard (a seat), and what
//! they do to the scene through the window manager.
//!
//! Backends translate their own events (winit, or libinput on the device) into
//! [`Input`], so dragging, raising and shortcuts behave the same everywhere and
//! are tested here without a device.

use protocol::NodeId;

use crate::scene::Scene;
use crate::wm::WindowManager;

/// The XKB keysym for Tab (`XKB_KEY_Tab`).
pub const KEY_TAB: u32 = 0xff09;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Input {
    /// Relative pointer motion from a mouse or touchpad, in pixels.
    Motion {
        dx: f32,
        dy: f32,
    },
    /// Absolute pointer position from a tablet or a hosted window, in pixels.
    MotionTo {
        x: f32,
        y: f32,
    },
    /// The primary (left) button.
    Button {
        pressed: bool,
    },
    Shortcut(Shortcut),
}

/// Keyboard shortcuts the compositor handles itself, before any app sees the key.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shortcut {
    /// Raise the bottom-most window, so repeating it visits every window.
    NextWindow,
}

/// The shortcut a key press triggers, if any. `keysym` is an XKB keysym.
pub fn shortcut(keysym: u32, alt: bool, logo: bool) -> Option<Shortcut> {
    match keysym {
        KEY_TAB if alt || logo => Some(Shortcut::NextWindow),
        _ => None,
    }
}

/// What one input changed, for the backend to act on.
#[derive(Debug, Default, PartialEq)]
pub struct Effect {
    /// The cursor moved, so a hardware cursor must follow it.
    pub cursor_moved: bool,
    /// A title-bar drag of this window ended.
    pub dropped: Option<NodeId>,
    /// A shortcut raised this window.
    pub raised: Option<NodeId>,
}

/// One pointer and keyboard. The cursor stays inside the screen.
#[derive(Debug)]
pub struct Seat {
    cursor: (f32, f32),
    bounds: (f32, f32),
}

impl Seat {
    /// A seat for a `w` by `h` screen, with the cursor in the middle.
    pub fn new(w: f32, h: f32) -> Seat {
        Seat { cursor: (w / 2.0, h / 2.0), bounds: (w, h) }
    }

    pub fn cursor(&self) -> (f32, f32) {
        self.cursor
    }

    /// The screen changed size; pull the cursor back inside it.
    pub fn set_bounds(&mut self, w: f32, h: f32) {
        self.bounds = (w, h);
        self.cursor = self.clamp(self.cursor.0, self.cursor.1);
    }

    pub fn handle(&mut self, input: Input, scene: &mut Scene, wm: &mut WindowManager) -> Effect {
        match input {
            Input::Motion { dx, dy } => self.move_to(self.cursor.0 + dx, self.cursor.1 + dy, scene, wm),
            Input::MotionTo { x, y } => self.move_to(x, y, scene, wm),
            Input::Button { pressed: true } => {
                wm.pointer_down(scene, self.cursor.0, self.cursor.1);
                Effect::default()
            }
            Input::Button { pressed: false } => Effect { dropped: wm.pointer_up(), ..Effect::default() },
            Input::Shortcut(Shortcut::NextWindow) => {
                Effect { raised: wm.raise_next(scene), ..Effect::default() }
            }
        }
    }

    fn clamp(&self, x: f32, y: f32) -> (f32, f32) {
        let max_x = (self.bounds.0 - 1.0).max(0.0);
        let max_y = (self.bounds.1 - 1.0).max(0.0);
        (x.clamp(0.0, max_x), y.clamp(0.0, max_y))
    }

    fn move_to(&mut self, x: f32, y: f32, scene: &mut Scene, wm: &mut WindowManager) -> Effect {
        let to = self.clamp(x, y);
        if to == self.cursor {
            return Effect::default();
        }
        self.cursor = to;
        if wm.is_dragging() {
            wm.pointer_move(scene, to.0, to.1);
        }
        Effect { cursor_moved: true, ..Effect::default() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo;

    fn desktop() -> (Scene, WindowManager, Seat) {
        let mut scene = Scene::new();
        scene.apply_all(demo::build(1280.0, 800.0, 3));
        scene.clear_dirty();
        (scene, WindowManager::default(), Seat::new(1280.0, 800.0))
    }

    fn windows(scene: &Scene) -> Vec<NodeId> {
        scene.top_level().iter().copied().filter(|&id| id >= 1000).collect()
    }

    #[test]
    fn relative_motion_stays_on_screen() {
        let (mut scene, mut wm, mut seat) = desktop();
        seat.handle(Input::Motion { dx: -5000.0, dy: 5000.0 }, &mut scene, &mut wm);
        assert_eq!(seat.cursor(), (0.0, 799.0));
        // Pushing further against the edge moves nothing.
        let effect = seat.handle(Input::Motion { dx: -1.0, dy: 1.0 }, &mut scene, &mut wm);
        assert_eq!(effect, Effect::default());
        seat.set_bounds(640.0, 480.0);
        assert_eq!(seat.cursor(), (0.0, 479.0));
    }

    #[test]
    fn dragging_a_title_bar_moves_the_window_and_reports_the_drop() {
        let (mut scene, mut wm, mut seat) = desktop();
        let top = *windows(&scene).last().unwrap();
        let r = scene.local_rect(top).unwrap();
        let grab = (r.x + 150.0, r.y + 10.0);

        seat.handle(Input::MotionTo { x: grab.0, y: grab.1 }, &mut scene, &mut wm);
        seat.handle(Input::Button { pressed: true }, &mut scene, &mut wm);
        // A mouse and a tablet drive the same drag.
        seat.handle(Input::Motion { dx: 40.0, dy: 20.0 }, &mut scene, &mut wm);
        let moved = seat.handle(Input::MotionTo { x: grab.0 + 100.0, y: grab.1 + 60.0 }, &mut scene, &mut wm);
        assert!(moved.cursor_moved);
        assert!(scene.is_dirty());
        let up = seat.handle(Input::Button { pressed: false }, &mut scene, &mut wm);
        assert_eq!(up.dropped, Some(top));

        let after = scene.local_rect(top).unwrap();
        assert_eq!((after.x, after.y), (r.x + 100.0, r.y + 60.0));
    }

    #[test]
    fn clicking_without_a_drag_drops_nothing() {
        let (mut scene, mut wm, mut seat) = desktop();
        seat.handle(Input::MotionTo { x: 5.0, y: 700.0 }, &mut scene, &mut wm); // empty desktop
        seat.handle(Input::Button { pressed: true }, &mut scene, &mut wm);
        let up = seat.handle(Input::Button { pressed: false }, &mut scene, &mut wm);
        assert_eq!(up.dropped, None);
        assert!(!scene.is_dirty());
    }

    #[test]
    fn next_window_visits_every_window_in_turn() {
        let (mut scene, mut wm, mut seat) = desktop();
        let start = windows(&scene);
        let mut raised = Vec::new();
        for _ in 0..start.len() {
            let effect = seat.handle(Input::Shortcut(Shortcut::NextWindow), &mut scene, &mut wm);
            raised.push(effect.raised.unwrap());
        }
        assert_eq!(raised, start, "each press raises the bottom-most window");
        assert_eq!(windows(&scene), start, "a full cycle restores the stacking order");
    }

    #[test]
    fn tab_is_a_shortcut_only_with_alt_or_logo() {
        assert_eq!(shortcut(KEY_TAB, false, false), None);
        assert_eq!(shortcut(KEY_TAB, true, false), Some(Shortcut::NextWindow));
        assert_eq!(shortcut(KEY_TAB, false, true), Some(Shortcut::NextWindow));
        assert_eq!(shortcut('a' as u32, true, true), None);
    }
}

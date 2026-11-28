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
    /// A key press (or a repeat). Releases aren't reported.
    Key {
        key: Key,
        mods: Mods,
    },
    /// Scrolling, in pixels: positive `dy` moves content up (a wheel turned
    /// toward you), as on every desktop.
    Scroll {
        dx: f32,
        dy: f32,
    },
    Shortcut(Shortcut),
    /// A media key (play/pause, next, volume). The compositor sends it to audiod;
    /// no app sees it.
    Media(MediaKey),
}

/// The keys a keyboard has for music, wherever the focus is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKey {
    /// Play/Pause (most keyboards' one play key), or a separate Play key.
    PlayPause,
    Pause,
    Stop,
    Next,
    Previous,
    VolumeUp,
    VolumeDown,
    Mute,
}

/// The media key for an XKB keysym (the `XF86Audio…` ones), if it is one.
pub fn media_from_keysym(keysym: u32) -> Option<MediaKey> {
    Some(match keysym {
        0x1008_ff14 => MediaKey::PlayPause, // XF86AudioPlay: what KEY_PLAYPAUSE gives
        0x1008_ff31 => MediaKey::Pause,     // XF86AudioPause
        0x1008_ff15 => MediaKey::Stop,      // XF86AudioStop
        0x1008_ff17 => MediaKey::Next,      // XF86AudioNext
        0x1008_ff16 => MediaKey::Previous,  // XF86AudioPrev
        0x1008_ff13 => MediaKey::VolumeUp,  // XF86AudioRaiseVolume
        0x1008_ff11 => MediaKey::VolumeDown, // XF86AudioLowerVolume
        0x1008_ff12 => MediaKey::Mute,      // XF86AudioMute
        _ => return None,
    })
}

/// A key, as text editing and shortcuts need it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Tab,
    Enter,
    Escape,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    /// A key that types this character, with Shift applied (`A`, `!`, ` `).
    Char(char),
}

/// Modifier keys held during a key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// Super or Command.
    pub logo: bool,
}

impl Mods {
    /// The editing-shortcut modifier: Command on a Mac keyboard, Ctrl elsewhere.
    pub fn primary(&self) -> bool {
        self.ctrl || self.logo
    }
}

/// The [`Key`] for an XKB keysym, given the character it types (`utf32`, 0 if
/// none). Used by the KMS backend; here so it's tested everywhere.
pub fn key_from_keysym(keysym: u32, utf32: u32) -> Option<Key> {
    Some(match keysym {
        0xff09 | 0xfe20 => Key::Tab, // Tab, ISO_Left_Tab (Shift+Tab)
        0xff0d | 0xff8d => Key::Enter,
        0xff1b => Key::Escape,
        0xff08 => Key::Backspace,
        0xffff => Key::Delete,
        0xff51 => Key::Left,
        0xff52 => Key::Up,
        0xff53 => Key::Right,
        0xff54 => Key::Down,
        0xff50 => Key::Home,
        0xff57 => Key::End,
        0xff55 => Key::PageUp,
        0xff56 => Key::PageDown,
        _ => match char::from_u32(utf32) {
            Some(c) if !c.is_control() => Key::Char(c),
            // With Ctrl held, xkb gives a control character (Ctrl-A is U+0001):
            // the key is still the letter, for Ctrl-A and the like.
            _ if (0x20..=0x7e).contains(&keysym) => Key::Char(char::from(keysym as u8)),
            _ => return None,
        },
    })
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

    /// The screen size the cursor is kept inside.
    pub fn bounds(&self) -> (f32, f32) {
        self.bounds
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
            // The desktop handles keys and scrolling before the seat sees input.
            Input::Key { .. } | Input::Scroll { .. } | Input::Media(_) => Effect::default(),
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
    fn keysyms_map_to_keys() {
        // Ctrl-A: xkb gives U+0001, but the key is still `a`.
        assert_eq!(key_from_keysym(0x61, 0x01), Some(Key::Char('a')));
        assert_eq!(key_from_keysym(0xff09, 0), Some(Key::Tab));
        assert_eq!(key_from_keysym(0xfe20, 0), Some(Key::Tab));
        assert_eq!(key_from_keysym(0xff51, 0), Some(Key::Left));
        assert_eq!(key_from_keysym('a' as u32, 'a' as u32), Some(Key::Char('a')));
        assert_eq!(key_from_keysym('A' as u32, 'A' as u32), Some(Key::Char('A')));
        assert_eq!(key_from_keysym(0x20ac, 0x20ac), Some(Key::Char('€')));
        assert_eq!(key_from_keysym(0xffe1, 0), None, "Shift alone types nothing");
        assert!(Mods { logo: true, ..Mods::default() }.primary());
    }

    #[test]
    fn media_keysyms_map_to_media_keys() {
        assert_eq!(media_from_keysym(0x1008_ff14), Some(MediaKey::PlayPause));
        assert_eq!(media_from_keysym(0x1008_ff17), Some(MediaKey::Next));
        assert_eq!(media_from_keysym(0x1008_ff16), Some(MediaKey::Previous));
        assert_eq!(media_from_keysym(0x1008_ff13), Some(MediaKey::VolumeUp));
        assert_eq!(media_from_keysym(0x1008_ff12), Some(MediaKey::Mute));
        assert_eq!(media_from_keysym('a' as u32), None);
        // Media keys type nothing, so they never reach apps as keys.
        assert_eq!(key_from_keysym(0x1008_ff14, 0), None);
    }

    #[test]
    fn tab_is_a_shortcut_only_with_alt_or_logo() {
        assert_eq!(shortcut(KEY_TAB, false, false), None);
        assert_eq!(shortcut(KEY_TAB, true, false), Some(Shortcut::NextWindow));
        assert_eq!(shortcut(KEY_TAB, false, true), Some(Shortcut::NextWindow));
        assert_eq!(shortcut('a' as u32, true, true), None);
    }
}

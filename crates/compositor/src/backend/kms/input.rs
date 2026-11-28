//! Keyboard and pointer devices through libinput, with keymaps from xkbcommon.
//!
//! libinput finds devices through udev (so a keyboard plugged in later works),
//! handles pointer acceleration and tablets, and hands over raw key codes;
//! xkbcommon turns those into keysyms with the configured layout. The result is
//! the backend-independent [`Input`] the seat understands.

use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use input::event::keyboard::{KeyState, KeyboardEventTrait};
use input::event::pointer::{Axis, ButtonState, PointerScrollEvent};
use input::event::{DeviceEvent, EventTrait, KeyboardEvent, PointerEvent};
use input::{DeviceCapability, Libinput, LibinputInterface};
use xkbcommon::xkb;

use crate::input::{Input, Mods, key_from_keysym, media_from_keysym};

/// Pixels one wheel click scrolls.
const WHEEL_STEP: f64 = 48.0;

/// `BTN_LEFT` from linux/input-event-codes.h.
const BTN_LEFT: u32 = 0x110;
/// XKB key codes are evdev key codes plus 8, a legacy of X11.
const EVDEV_TO_XKB: u32 = 8;

/// Opens the device nodes libinput asks for. The compositor runs as root, so no
/// privilege broker (logind, seatd) is involved.
struct Opener;

impl LibinputInterface for Opener {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        let access = flags & libc::O_ACCMODE;
        OpenOptions::new()
            .custom_flags(flags)
            .read(access == libc::O_RDONLY || access == libc::O_RDWR)
            .write(access == libc::O_WRONLY || access == libc::O_RDWR)
            .open(path)
            .map(OwnedFd::from)
            .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))
    }

    fn close_restricted(&mut self, fd: OwnedFd) {
        drop(File::from(fd));
    }
}

pub struct Devices {
    libinput: Libinput,
    keyboard: xkb::State,
}

impl Devices {
    /// Open every input device on `seat` (normally `seat0`) and compile the keymap.
    /// The layout follows `XKB_DEFAULT_LAYOUT` and friends, defaulting to US.
    pub fn open(seat: &str) -> Result<Devices, String> {
        let mut libinput = Libinput::new_with_udev(Opener);
        libinput.udev_assign_seat(seat).map_err(|()| format!("libinput: cannot assign {seat}"))?;
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(&context, "", "", "", "", None, xkb::COMPILE_NO_FLAGS)
            .ok_or("xkbcommon: cannot compile the default keymap (is xkeyboard-config installed?)")?;
        Ok(Devices { libinput, keyboard: xkb::State::new(&keymap) })
    }

    /// Readable when libinput has events; poll it with the display.
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.libinput.as_fd()
    }

    /// Read every pending event, appending the ones the compositor acts on to `out`.
    /// `screen` maps absolute devices (tablets, QEMU's pointer) to pixels.
    pub fn read(&mut self, screen: (u32, u32), out: &mut Vec<Input>) -> Result<(), String> {
        self.libinput.dispatch().map_err(|e| format!("libinput dispatch: {e}"))?;
        for event in &mut self.libinput {
            match event {
                input::Event::Device(DeviceEvent::Added(e)) => {
                    let device = e.device();
                    log::info!("input: added {} ({})", device.name(), capabilities(&device));
                }
                input::Event::Device(DeviceEvent::Removed(e)) => {
                    log::info!("input: removed {}", e.device().name());
                }
                input::Event::Pointer(PointerEvent::Motion(m)) => {
                    out.push(Input::Motion { dx: m.dx() as f32, dy: m.dy() as f32 });
                }
                input::Event::Pointer(PointerEvent::MotionAbsolute(m)) => out.push(Input::MotionTo {
                    x: m.absolute_x_transformed(screen.0) as f32,
                    y: m.absolute_y_transformed(screen.1) as f32,
                }),
                input::Event::Pointer(PointerEvent::Button(b)) if b.button() == BTN_LEFT => {
                    out.push(Input::Button { pressed: b.button_state() == ButtonState::Pressed });
                }
                input::Event::Pointer(PointerEvent::ScrollWheel(e)) => {
                    let v = |axis| {
                        if e.has_axis(axis) { e.scroll_value_v120(axis) / 120.0 * WHEEL_STEP } else { 0.0 }
                    };
                    out.push(Input::Scroll { dx: v(Axis::Horizontal) as f32, dy: v(Axis::Vertical) as f32 });
                }
                input::Event::Pointer(PointerEvent::ScrollFinger(e)) => {
                    let v = |axis| if e.has_axis(axis) { e.scroll_value(axis) } else { 0.0 };
                    out.push(Input::Scroll { dx: v(Axis::Horizontal) as f32, dy: v(Axis::Vertical) as f32 });
                }
                input::Event::Pointer(PointerEvent::ScrollContinuous(e)) => {
                    let v = |axis| if e.has_axis(axis) { e.scroll_value(axis) } else { 0.0 };
                    out.push(Input::Scroll { dx: v(Axis::Horizontal) as f32, dy: v(Axis::Vertical) as f32 });
                }
                input::Event::Keyboard(KeyboardEvent::Key(k)) => {
                    let code = xkb::Keycode::new(k.key() + EVDEV_TO_XKB);
                    let pressed = k.key_state() == KeyState::Pressed;
                    if pressed {
                        // Look up the symbol before updating, so a modifier's own
                        // press doesn't count toward itself. The character comes from
                        // the keysym, so Ctrl+C still reads as "c".
                        let sym = self.keyboard.key_get_one_sym(code);
                        let mods = Mods {
                            ctrl: modifier(&self.keyboard, xkb::MOD_NAME_CTRL),
                            alt: modifier(&self.keyboard, xkb::MOD_NAME_ALT),
                            shift: modifier(&self.keyboard, xkb::MOD_NAME_SHIFT),
                            logo: modifier(&self.keyboard, xkb::MOD_NAME_LOGO),
                        };
                        if let Some(media) = media_from_keysym(sym.raw()) {
                            out.push(Input::Media(media));
                        } else if let Some(key) = key_from_keysym(sym.raw(), xkb::keysym_to_utf32(sym)) {
                            out.push(Input::Key { key, mods });
                        }
                    }
                    let direction = if pressed { xkb::KeyDirection::Down } else { xkb::KeyDirection::Up };
                    self.keyboard.update_key(code, direction);
                }
                _ => {}
            }
        }
        Ok(())
    }
}

fn modifier(keyboard: &xkb::State, name: &str) -> bool {
    keyboard.mod_name_is_active(name, xkb::STATE_MODS_EFFECTIVE)
}

fn capabilities(device: &input::Device) -> String {
    let names = [
        (DeviceCapability::Keyboard, "keyboard"),
        (DeviceCapability::Pointer, "pointer"),
        (DeviceCapability::Touch, "touch"),
        (DeviceCapability::TabletTool, "tablet"),
    ];
    let found: Vec<&str> =
        names.iter().filter(|(cap, _)| device.has_capability(*cap)).map(|(_, name)| *name).collect();
    if found.is_empty() { "no capabilities the compositor uses".into() } else { found.join(", ") }
}

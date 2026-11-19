//! Keyboard (SimpleTextInputEx on the console-in handle; plain
//! SimpleTextInput as fallback) and pointer (AbsolutePointer, else
//! SimplePointer, else none).

use alloc::collections::VecDeque;
use pos_hal::{InputEvent, Key, Modifiers};
use uefi::boot::{self, ScopedProtocol};
use uefi::proto::console::pointer::{AbsolutePointer, Pointer};
use uefi::proto::console::text::{Input, InputEx, Key as UKey, ScanCode};
use uefi::{Event, Handle};

use crate::clock::TscClock;
use crate::open_shared;

enum Kbd {
    Ex(ScopedProtocol<InputEx>),
    Plain(ScopedProtocol<Input>),
}

enum Ptr {
    Abs(ScopedProtocol<AbsolutePointer>, u64, u64, u64, u64),
    Rel(ScopedProtocol<Pointer>),
}

pub struct UefiInput {
    kbd: Kbd,
    ptr: Option<Ptr>,
    width: u32,
    height: u32,
    x: i64,
    y: i64,
    pressed: bool,
    queue: VecDeque<InputEvent>,
}

fn console_in_handle() -> Option<Handle> {
    let st = uefi::table::system_table_raw()?;
    // SAFETY: the system table pointer is valid while boot services run.
    let h = unsafe { st.as_ref().stdin_handle };
    // SAFETY: a non-null handle from the system table is a valid handle.
    unsafe { Handle::from_ptr(h) }
}

impl UefiInput {
    pub fn init(width: u32, height: u32) -> Option<UefiInput> {
        let h = console_in_handle()?;
        let kbd = match open_shared::<InputEx>(h) {
            Some(p) => Kbd::Ex(p),
            None => Kbd::Plain(open_shared::<Input>(h)?),
        };
        // Pointer: prefer absolute (touch screens, QEMU usb-tablet).
        let ptr = boot::find_handles::<AbsolutePointer>()
            .ok()
            .and_then(|hs| hs.first().copied())
            .and_then(open_shared::<AbsolutePointer>)
            .map(|p| {
                let (min_x, max_x, min_y, max_y) = abs_range(&p);
                Ptr::Abs(p, min_x, max_x, min_y, max_y)
            })
            .or_else(|| {
                boot::find_handles::<Pointer>()
                    .ok()
                    .and_then(|hs| hs.first().copied())
                    .and_then(open_shared::<Pointer>)
                    .map(Ptr::Rel)
            });
        Some(UefiInput {
            kbd,
            ptr,
            width,
            height,
            x: i64::from(width / 2),
            y: i64::from(height / 2),
            pressed: false,
            queue: VecDeque::new(),
        })
    }

    /// Events to wait on: keyboard first, then pointer.
    pub fn wait_events(&self) -> alloc::vec::Vec<Event> {
        let mut v = alloc::vec::Vec::new();
        let k = match &self.kbd {
            Kbd::Ex(p) => p.wait_for_key_event().ok(),
            Kbd::Plain(p) => p.wait_for_key_event().ok(),
        };
        v.extend(k);
        let p = match &self.ptr {
            Some(Ptr::Abs(p, ..)) => p.wait_for_input_event().ok(),
            Some(Ptr::Rel(p)) => p.wait_for_input_event().ok(),
            None => None,
        };
        v.extend(p);
        v
    }

    /// Drain firmware input into the queue, time-stamping each event.
    pub fn pump(&mut self, clock: &TscClock) {
        loop {
            let (key, mods) = match &mut self.kbd {
                Kbd::Ex(p) => match p.read_key() {
                    Ok(Some(kd)) => {
                        let mut m = Modifiers::default();
                        if let Some(s) = kd.key_state.key_shift_state {
                            use uefi_raw::protocol::console::KeyShiftState as S;
                            m.shift = s.intersects(S::LEFT_SHIFT | S::RIGHT_SHIFT);
                            m.ctrl = s.intersects(S::LEFT_CONTROL | S::RIGHT_CONTROL);
                            m.alt = s.intersects(S::LEFT_ALT | S::RIGHT_ALT);
                        }
                        (kd.key, m)
                    }
                    _ => break,
                },
                Kbd::Plain(p) => match p.read_key() {
                    Ok(Some(k)) => (k, Modifiers::default()),
                    _ => break,
                },
            };
            if let Some(k) = map_key(key) {
                let at_us = pos_hal::Clock::monotonic_us(clock);
                self.queue.push_back(InputEvent::Key {
                    key: k,
                    mods,
                    at_us,
                });
            }
        }
        self.pump_pointer(clock);
    }

    fn pump_pointer(&mut self, clock: &TscClock) {
        let (w, h) = (i64::from(self.width), i64::from(self.height));
        let ev = match &mut self.ptr {
            Some(Ptr::Abs(p, min_x, max_x, min_y, max_y)) => match p.read_state() {
                Ok(Some(s)) => {
                    let sx = (max_x.saturating_sub(*min_x)).max(1);
                    let sy = (max_y.saturating_sub(*min_y)).max(1);
                    self.x = ((s.current_x.saturating_sub(*min_x)) as i64 * (w - 1) / sx as i64)
                        .clamp(0, w - 1);
                    self.y = ((s.current_y.saturating_sub(*min_y)) as i64 * (h - 1) / sy as i64)
                        .clamp(0, h - 1);
                    Some(s.active_buttons & 1 != 0)
                }
                _ => None,
            },
            Some(Ptr::Rel(p)) => match p.read_state() {
                Ok(Some(s)) => {
                    // Relative mice: move ~1 px per count, clamp to screen.
                    self.x = (self.x + i64::from(s.relative_movement_x)).clamp(0, w - 1);
                    self.y = (self.y + i64::from(s.relative_movement_y)).clamp(0, h - 1);
                    Some(bool::from(s.left_button))
                }
                _ => None,
            },
            None => None,
        };
        if let Some(pressed) = ev
            && pressed != self.pressed
        {
            self.pressed = pressed;
            self.queue.push_back(InputEvent::Pointer {
                x: self.x as u32,
                y: self.y as u32,
                pressed,
                at_us: pos_hal::Clock::monotonic_us(clock),
            });
        }
    }
}

fn abs_range(p: &ScopedProtocol<AbsolutePointer>) -> (u64, u64, u64, u64) {
    // The `uefi` wrapper does not expose the mode; read it from the raw
    // protocol struct, which `AbsolutePointer` wraps transparently.
    let raw: *const AbsolutePointer = &**p;
    // SAFETY: `AbsolutePointer` is `#[repr(transparent)]` over
    // `uefi_raw::protocol::console::AbsolutePointerProtocol`, and the firmware
    // keeps `mode` pointing at valid memory while the protocol is open.
    unsafe {
        let proto = &*(raw.cast::<uefi_raw::protocol::console::AbsolutePointerProtocol>());
        match proto.mode.as_ref() {
            Some(m) => (
                m.absolute_min_x,
                m.absolute_max_x,
                m.absolute_min_y,
                m.absolute_max_y,
            ),
            None => (0, 0xFFFF, 0, 0xFFFF),
        }
    }
}

fn map_key(k: UKey) -> Option<Key> {
    Some(match k {
        UKey::Printable(c) => {
            let ch = char::from(c);
            match ch {
                '\r' | '\n' => Key::Enter,
                '\u{8}' => Key::Backspace,
                '\t' => Key::Tab,
                c if (c as u32) < 0x20 => return None,
                c => Key::Char(c),
            }
        }
        UKey::Special(s) => match s {
            ScanCode::UP => Key::Up,
            ScanCode::DOWN => Key::Down,
            ScanCode::LEFT => Key::Left,
            ScanCode::RIGHT => Key::Right,
            ScanCode::HOME => Key::Home,
            ScanCode::END => Key::End,
            ScanCode::INSERT => Key::Insert,
            ScanCode::DELETE => Key::Delete,
            ScanCode::PAGE_UP => Key::PageUp,
            ScanCode::PAGE_DOWN => Key::PageDown,
            ScanCode::ESCAPE => Key::Escape,
            s if (ScanCode::FUNCTION_1.0..=ScanCode::FUNCTION_12.0).contains(&s.0) => {
                Key::F((s.0 - ScanCode::FUNCTION_1.0 + 1) as u8)
            }
            _ => return None,
        },
    })
}

impl pos_hal::Input for UefiInput {
    fn poll(&mut self) -> Option<InputEvent> {
        self.queue.pop_front()
    }
    fn has_pointer(&self) -> bool {
        self.ptr.is_some()
    }
}

//! macOS backend: CGEventTap for capture, CGEventPost for injection.
//! Needs Accessibility (and Input Monitoring for the server) permission in
//! System Settings > Privacy & Security for the terminal or glide binary.

use super::{CaptureHandler, InputEvent, RawInjector};
use crate::keymap;
use crate::proto::*;
use core_foundation::base::TCFType;
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop, CFRunLoopTimer, CFRunLoopTimerRef};
use core_graphics::display::{CGDisplay, CGPoint};
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType, CGMouseButton, EventField, ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::time::Instant;

thread_local! {
    static HANDLER: RefCell<Option<Box<dyn CaptureHandler>>> = const { RefCell::new(None) };
    static MODS: Cell<u64> = const { Cell::new(0) };
}
static TAP_PORT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static RUN_LOOP: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventTapEnable(tap: *mut c_void, enable: bool);
    fn CGAssociateMouseAndMouseCursorPosition(connected: u32) -> i32;
    fn CGWarpMouseCursorPosition(p: CGPoint) -> i32;
    fn CGDisplayHideCursor(display: u32) -> i32;
    fn CGDisplayShowCursor(display: u32) -> i32;
    fn _CGSDefaultConnection() -> i32;
    fn CGSSetConnectionProperty(cid: i32, target: i32, key: *const c_void, value: *const c_void) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFAbsoluteTimeGetCurrent() -> f64;
    fn CFRunLoopStop(rl: *mut c_void);
    static kCFBooleanTrue: *const c_void;
}

pub fn init_process() {}

pub fn desktop_bounds() -> Rect {
    let ids = CGDisplay::active_displays().unwrap_or_default();
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for id in ids {
        let b = CGDisplay::new(id).bounds();
        x0 = x0.min(b.origin.x);
        y0 = y0.min(b.origin.y);
        x1 = x1.max(b.origin.x + b.size.width);
        y1 = y1.max(b.origin.y + b.size.height);
    }
    if x0 > x1 {
        return Rect { x: 0, y: 0, w: 1440, h: 900 };
    }
    Rect { x: x0 as i32, y: y0 as i32, w: (x1 - x0) as i32, h: (y1 - y0) as i32 }
}

pub fn boost_thread() {
    // Ask for the latency-sensitive QoS class (QOS_CLASS_USER_INTERACTIVE).
    extern "C" {
        fn pthread_set_qos_class_self_np(qos: u32, rel: i32) -> i32;
    }
    unsafe {
        pthread_set_qos_class_self_np(0x21, 0);
    }
}

pub fn grab(on: bool, pos: (i32, i32)) {
    unsafe {
        let main = CGDisplay::main().id;
        if on {
            // Lets a background process hide the cursor (what Synergy-style tools use).
            let key = core_foundation::string::CFString::new("SetsCursorInBackground");
            let cid = _CGSDefaultConnection();
            CGSSetConnectionProperty(cid, cid, key.as_concrete_TypeRef() as *const c_void, kCFBooleanTrue);
            CGDisplayHideCursor(main);
            CGAssociateMouseAndMouseCursorPosition(0);
        } else {
            CGWarpMouseCursorPosition(CGPoint::new(pos.0 as f64, pos.1 as f64));
            CGAssociateMouseAndMouseCursorPosition(1);
            CGDisplayShowCursor(main);
        }
    }
}

/// Ends `run_capture` (called from another thread).
pub fn stop_capture() {
    let rl = RUN_LOOP.swap(std::ptr::null_mut(), Ordering::Relaxed);
    if !rl.is_null() {
        unsafe { CFRunLoopStop(rl) };
    }
}

pub fn restore_cursor() {
    unsafe {
        CGAssociateMouseAndMouseCursorPosition(1);
        CGDisplayShowCursor(CGDisplay::main().id);
    }
}

fn dispatch(ev: InputEvent) -> bool {
    HANDLER.with(|h| match h.try_borrow_mut() {
        Ok(mut h) => h.as_mut().map(|h| h.event(ev)).unwrap_or(false),
        Err(_) => false,
    })
}

fn is_remote() -> bool {
    HANDLER.with(|h| h.try_borrow().ok().and_then(|h| h.as_ref().map(|h| h.remote())).unwrap_or(false))
}

/// Device-dependent modifier bits (NX_DEVICE*KEYMASK) per HID modifier.
const MOD_BITS: [(u16, u64); 8] = [
    (keymap::HID_LCTRL, 0x0001),
    (keymap::HID_LSHIFT, 0x0002),
    (keymap::HID_RSHIFT, 0x0004),
    (keymap::HID_LGUI, 0x0008),
    (keymap::HID_RGUI, 0x0010),
    (keymap::HID_LALT, 0x0020),
    (keymap::HID_RALT, 0x0040),
    (keymap::HID_RCTRL, 0x2000),
];

fn translate(ty: CGEventType, ev: &CGEvent) -> Option<InputEvent> {
    use CGEventType::*;
    let btn = |b| Some(InputEvent::Button { button: b, down: false });
    match ty {
        MouseMoved | LeftMouseDragged | RightMouseDragged | OtherMouseDragged => {
            let p = ev.location();
            Some(InputEvent::Motion {
                x: p.x as i32,
                y: p.y as i32,
                dx: ev.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_X) as i32,
                dy: ev.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y) as i32,
            })
        }
        LeftMouseDown => Some(InputEvent::Button { button: BTN_LEFT, down: true }),
        LeftMouseUp => btn(BTN_LEFT),
        RightMouseDown => Some(InputEvent::Button { button: BTN_RIGHT, down: true }),
        RightMouseUp => btn(BTN_RIGHT),
        OtherMouseDown | OtherMouseUp => {
            let b = match ev.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER) {
                2 => BTN_MIDDLE,
                3 => BTN_X1,
                4 => BTN_X2,
                _ => return None,
            };
            Some(InputEvent::Button { button: b, down: matches!(ty, OtherMouseDown) })
        }
        ScrollWheel => {
            let dy = ev.get_double_value_field(EventField::SCROLL_WHEEL_EVENT_FIXED_POINT_DELTA_AXIS_1);
            let dx = ev.get_double_value_field(EventField::SCROLL_WHEEL_EVENT_FIXED_POINT_DELTA_AXIS_2);
            Some(InputEvent::Wheel { dx: (dx * 120.0) as i32, dy: (dy * 120.0) as i32 })
        }
        KeyDown | KeyUp => {
            let code = ev.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
            keymap::hid_from_mac(code).map(|hid| InputEvent::Key { hid, down: matches!(ty, KeyDown) })
        }
        FlagsChanged => {
            let code = ev.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
            let hid = keymap::hid_from_mac(code)?;
            let flags = ev.get_flags().bits();
            let prev = MODS.with(|m| m.replace(flags));
            let bit = MOD_BITS.iter().find(|(h, _)| *h == hid).map(|(_, b)| *b);
            let down = match bit {
                Some(b) => flags & b != 0,
                None => flags & CGEventFlags::CGEventFlagAlphaShift.bits() != prev & CGEventFlags::CGEventFlagAlphaShift.bits(),
            };
            Some(InputEvent::Key { hid, down })
        }
        _ => None,
    }
}

extern "C" fn on_timer(_t: CFRunLoopTimerRef, _info: *mut c_void) {
    HANDLER.with(|h| {
        if let Ok(mut h) = h.try_borrow_mut() {
            if let Some(h) = h.as_mut() {
                h.tick();
            }
        }
    });
}

pub fn run_capture(handler: Box<dyn CaptureHandler>) -> Result<(), String> {
    use CGEventType::*;
    boost_thread();
    HANDLER.with(|h| *h.borrow_mut() = Some(handler));
    let tap = CGEventTap::new(
        CGEventTapLocation::HID,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::Default,
        vec![
            MouseMoved, LeftMouseDown, LeftMouseUp, RightMouseDown, RightMouseUp, OtherMouseDown, OtherMouseUp,
            LeftMouseDragged, RightMouseDragged, OtherMouseDragged, ScrollWheel, KeyDown, KeyUp, FlagsChanged,
        ],
        |_proxy, ty, ev| {
            if matches!(ty, TapDisabledByTimeout | TapDisabledByUserInput) {
                let p = TAP_PORT.load(Ordering::Relaxed);
                if !p.is_null() {
                    unsafe { CGEventTapEnable(p, true) };
                }
                return Some(ev.clone());
            }
            let swallow = match translate(ty, ev) {
                Some(e) => dispatch(e),
                None => matches!(ty, KeyDown | KeyUp | FlagsChanged) && is_remote(),
            };
            if swallow {
                None
            } else {
                Some(ev.clone())
            }
        },
    )
    .map_err(|_| {
        "could not create the event tap: grant Accessibility and Input Monitoring to this app \
         in System Settings > Privacy & Security, then restart glide"
            .to_string()
    })?;
    TAP_PORT.store(tap.mach_port.as_concrete_TypeRef() as *mut c_void, Ordering::Relaxed);
    let src = tap.mach_port.create_runloop_source(0).map_err(|_| "runloop source")?;
    let rl = CFRunLoop::get_current();
    unsafe {
        rl.add_source(&src, kCFRunLoopCommonModes);
        let timer = CFRunLoopTimer::new(CFAbsoluteTimeGetCurrent() + 0.1, 0.1, 0, 0, on_timer, std::ptr::null_mut());
        rl.add_timer(&timer, kCFRunLoopCommonModes);
    }
    tap.enable();
    RUN_LOOP.store(rl.as_concrete_TypeRef() as *mut c_void, Ordering::Relaxed);
    CFRunLoop::run_current();
    TAP_PORT.store(std::ptr::null_mut(), Ordering::Relaxed);
    HANDLER.with(|h| *h.borrow_mut() = None);
    restore_cursor();
    Ok(())
}

pub struct Injector {
    src: CGEventSource,
    held: HashSet<u8>,
    pos: (f64, f64),
    mods: HashSet<u16>,
    wheel_acc: (i32, i32),
    last_click: (Instant, u8, (f64, f64), i64),
}

// The event source is only used behind the client's Mutex; CGEventPost is thread-safe.
unsafe impl Send for Injector {}

impl Injector {
    pub fn new() -> Self {
        let src = CGEventSource::new(CGEventSourceStateID::HIDSystemState).expect("CGEventSource");
        Injector {
            src,
            held: HashSet::new(),
            pos: (0.0, 0.0),
            mods: HashSet::new(),
            wheel_acc: (0, 0),
            last_click: (Instant::now(), 0, (0.0, 0.0), 0),
        }
    }

    fn flags(&self) -> CGEventFlags {
        let mut f = CGEventFlags::CGEventFlagNull;
        let has = |a, b| self.mods.contains(&a) || self.mods.contains(&b);
        if has(keymap::HID_LSHIFT, keymap::HID_RSHIFT) {
            f |= CGEventFlags::CGEventFlagShift;
        }
        if has(keymap::HID_LCTRL, keymap::HID_RCTRL) {
            f |= CGEventFlags::CGEventFlagControl;
        }
        if has(keymap::HID_LALT, keymap::HID_RALT) {
            f |= CGEventFlags::CGEventFlagAlternate;
        }
        if has(keymap::HID_LGUI, keymap::HID_RGUI) {
            f |= CGEventFlags::CGEventFlagCommand;
        }
        f
    }

    fn post_mouse(&self, ty: CGEventType, button: CGMouseButton, number: i64, click_state: i64) {
        if let Ok(e) = CGEvent::new_mouse_event(self.src.clone(), ty, CGPoint::new(self.pos.0, self.pos.1), button) {
            e.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, number);
            if click_state > 0 {
                e.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, click_state);
            }
            e.set_flags(self.flags());
            e.post(CGEventTapLocation::HID);
        }
    }
}

impl RawInjector for Injector {
    fn move_abs(&mut self, x: i32, y: i32) {
        let (ox, oy) = self.pos;
        self.pos = (x as f64, y as f64);
        let (ty, b) = if self.held.contains(&BTN_LEFT) {
            (CGEventType::LeftMouseDragged, CGMouseButton::Left)
        } else if self.held.contains(&BTN_RIGHT) {
            (CGEventType::RightMouseDragged, CGMouseButton::Right)
        } else if !self.held.is_empty() {
            (CGEventType::OtherMouseDragged, CGMouseButton::Center)
        } else {
            (CGEventType::MouseMoved, CGMouseButton::Left)
        };
        if let Ok(e) = CGEvent::new_mouse_event(self.src.clone(), ty, CGPoint::new(self.pos.0, self.pos.1), b) {
            // Deltas for apps/games that read relative motion.
            e.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_X, (self.pos.0 - ox) as i64);
            e.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y, (self.pos.1 - oy) as i64);
            e.set_flags(self.flags());
            e.post(CGEventTapLocation::HID);
        }
    }

    fn button(&mut self, button: u8, down: bool) {
        use CGEventType::*;
        let (ty, b, num) = match (button, down) {
            (BTN_LEFT, true) => (LeftMouseDown, CGMouseButton::Left, 0),
            (BTN_LEFT, false) => (LeftMouseUp, CGMouseButton::Left, 0),
            (BTN_RIGHT, true) => (RightMouseDown, CGMouseButton::Right, 1),
            (BTN_RIGHT, false) => (RightMouseUp, CGMouseButton::Right, 1),
            (BTN_MIDDLE, d) => (if d { OtherMouseDown } else { OtherMouseUp }, CGMouseButton::Center, 2),
            (BTN_X1, d) => (if d { OtherMouseDown } else { OtherMouseUp }, CGMouseButton::Center, 3),
            (BTN_X2, d) => (if d { OtherMouseDown } else { OtherMouseUp }, CGMouseButton::Center, 4),
            _ => return,
        };
        // macOS needs an explicit click count for double/triple clicks.
        let (t, lb, lp, count) = self.last_click;
        let click_state = if down {
            let near = (lp.0 - self.pos.0).abs() <= 4.0 && (lp.1 - self.pos.1).abs() <= 4.0;
            let c = if lb == button && near && t.elapsed().as_millis() < 500 { count + 1 } else { 1 };
            self.last_click = (Instant::now(), button, self.pos, c);
            self.held.insert(button);
            c
        } else {
            self.held.remove(&button);
            count.max(1)
        };
        self.post_mouse(ty, b, num, click_state);
    }

    fn wheel(&mut self, dx: i32, dy: i32) {
        self.wheel_acc.0 += dx;
        self.wheel_acc.1 += dy;
        let lx = self.wheel_acc.0 / 120;
        let ly = self.wheel_acc.1 / 120;
        if lx == 0 && ly == 0 {
            return;
        }
        self.wheel_acc.0 -= lx * 120;
        self.wheel_acc.1 -= ly * 120;
        if let Ok(e) = CGEvent::new_scroll_event(self.src.clone(), ScrollEventUnit::LINE, 2, ly, lx, 0) {
            e.set_flags(self.flags());
            e.post(CGEventTapLocation::HID);
        }
    }

    fn key(&mut self, hid: u16, down: bool) {
        let Some(code) = keymap::mac_from_hid(hid) else { return };
        if (keymap::HID_LCTRL..=keymap::HID_RGUI).contains(&hid) {
            if down {
                self.mods.insert(hid);
            } else {
                self.mods.remove(&hid);
            }
        }
        if let Ok(e) = CGEvent::new_keyboard_event(self.src.clone(), code, down) {
            e.set_flags(self.flags());
            e.post(CGEventTapLocation::HID);
        }
    }
}

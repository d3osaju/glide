//! Windows backend: WH_MOUSE_LL / WH_KEYBOARD_LL hooks for capture, SendInput
//! with hardware scan codes for injection, SetSystemCursor to hide the cursor.
//!
//! Hook callbacks run on the capture thread and must return fast (Windows
//! silently drops hooks that exceed LowLevelHooksTimeout), so all they do is
//! translate the event and hand it to the router, which only does in-memory
//! work plus a non-blocking UDP send or a channel push.

use super::{CaptureHandler, InputEvent, RawInjector};
use crate::keymap;
use crate::proto::*;
use std::cell::{Cell, RefCell};
use std::mem::size_of;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU32, Ordering};

static THREAD: AtomicU32 = AtomicU32::new(0);
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const WM_APPLY_GRAB: u32 = WM_APP + 1;

thread_local! {
    static HANDLER: RefCell<Option<Box<dyn CaptureHandler>>> = const { RefCell::new(None) };
    /// Cursor position as Windows currently has it (unchanged while we swallow moves).
    static LAST: Cell<(i32, i32)> = const { Cell::new((0, 0)) };
    /// Grab request applied after the hook returns: (hide, warp target).
    static PENDING: Cell<Option<(bool, (i32, i32))>> = const { Cell::new(None) };
}

pub fn init_process() {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        // Stay responsive when torrents, games or builds load the CPU.
        SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS);
    }
    // A previous crash may have left the cursor hidden.
    restore_cursor();
    // ...or the speakers muted.
    super::winaudio::recover();
}

pub fn desktop_bounds() -> Rect {
    unsafe {
        Rect {
            x: GetSystemMetrics(SM_XVIRTUALSCREEN),
            y: GetSystemMetrics(SM_YVIRTUALSCREEN),
            w: GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
            h: GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
        }
    }
}

pub fn boost_thread() {
    unsafe {
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
    }
}

/// Hide + park (on=true) or show + warp to `pos` (on=false). Deferred until the
/// current hook callback returns so we never warp from inside the hook.
pub fn grab(on: bool, pos: (i32, i32)) {
    PENDING.with(|p| p.set(Some((on, pos))));
    let tid = THREAD.load(Ordering::Relaxed);
    unsafe {
        PostThreadMessageW(tid, WM_APPLY_GRAB, 0, 0);
    }
}

fn apply_grab() {
    let Some((on, pos)) = PENDING.with(|p| p.take()) else { return };
    unsafe {
        if on {
            hide_cursor();
            // Park on the primary monitor's centre, away from every edge, so
            // the hook keeps reporting full deltas.
            let cx = GetSystemMetrics(SM_CXSCREEN) / 2;
            let cy = GetSystemMetrics(SM_CYSCREEN) / 2;
            SetCursorPos(cx, cy);
        } else {
            SetCursorPos(pos.0, pos.1);
            restore_cursor();
        }
        let mut p = POINT { x: 0, y: 0 };
        GetCursorPos(&mut p);
        LAST.with(|l| l.set((p.x, p.y)));
    }
}

const CURSOR_IDS: [u32; 14] = [
    OCR_NORMAL, OCR_IBEAM, OCR_WAIT, OCR_CROSS, OCR_UP, OCR_SIZENWSE, OCR_SIZENESW, OCR_SIZEWE,
    OCR_SIZENS, OCR_SIZEALL, OCR_NO, OCR_HAND, OCR_APPSTARTING, OCR_HELP,
];

unsafe fn hide_cursor() {
    let and = [0xFFu8; 128];
    let xor = [0u8; 128];
    let hinst = GetModuleHandleW(null());
    for id in CURSOR_IDS {
        // SetSystemCursor takes ownership of (and destroys) the handle, so one per id.
        let c = CreateCursor(hinst, 0, 0, 32, 32, and.as_ptr() as _, xor.as_ptr() as _);
        if !c.is_null() {
            SetSystemCursor(c, id);
        }
    }
}

/// Ends `run_capture` (called from another thread).
pub fn stop_capture() {
    let tid = THREAD.swap(0, Ordering::Relaxed);
    if tid != 0 {
        unsafe {
            PostThreadMessageW(tid, WM_QUIT, 0, 0);
        }
    }
}

pub fn restore_cursor() {
    unsafe {
        SystemParametersInfoW(SPI_SETCURSORS, 0, null_mut(), 0);
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

unsafe extern "system" fn mouse_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let ms = &*(lp as *const MSLLHOOKSTRUCT);
        if ms.flags & LLMHF_INJECTED == 0 {
            let hi = (ms.mouseData >> 16) as u16;
            let ev = match wp as u32 {
                WM_MOUSEMOVE => {
                    let (lx, ly) = LAST.with(|l| l.get());
                    Some(InputEvent::Motion { x: ms.pt.x, y: ms.pt.y, dx: ms.pt.x - lx, dy: ms.pt.y - ly })
                }
                WM_LBUTTONDOWN => Some(InputEvent::Button { button: BTN_LEFT, down: true }),
                WM_LBUTTONUP => Some(InputEvent::Button { button: BTN_LEFT, down: false }),
                WM_RBUTTONDOWN => Some(InputEvent::Button { button: BTN_RIGHT, down: true }),
                WM_RBUTTONUP => Some(InputEvent::Button { button: BTN_RIGHT, down: false }),
                WM_MBUTTONDOWN => Some(InputEvent::Button { button: BTN_MIDDLE, down: true }),
                WM_MBUTTONUP => Some(InputEvent::Button { button: BTN_MIDDLE, down: false }),
                WM_XBUTTONDOWN | WM_XBUTTONUP => Some(InputEvent::Button {
                    button: if hi == 2 { BTN_X2 } else { BTN_X1 },
                    down: wp as u32 == WM_XBUTTONDOWN,
                }),
                WM_MOUSEWHEEL => Some(InputEvent::Wheel { dx: 0, dy: hi as i16 as i32 }),
                WM_MOUSEHWHEEL => Some(InputEvent::Wheel { dx: hi as i16 as i32, dy: 0 }),
                _ => None,
            };
            if let Some(ev) = ev {
                let swallow = dispatch(ev);
                if let InputEvent::Motion { x, y, .. } = ev {
                    if !swallow {
                        LAST.with(|l| l.set((x, y)));
                    }
                }
                if swallow {
                    return 1;
                }
            }
        }
    }
    CallNextHookEx(null_mut(), code, wp, lp)
}

unsafe extern "system" fn keyboard_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let kb = &*(lp as *const KBDLLHOOKSTRUCT);
        if kb.flags & LLKHF_INJECTED == 0 {
            let down = kb.flags & LLKHF_UP == 0;
            let hid = match kb.vkCode as u16 {
                VK_PAUSE => Some(keymap::HID_PAUSE),
                VK_NUMLOCK => Some(keymap::HID_NUMLOCK),
                VK_VOLUME_MUTE => Some(0x7F),
                VK_VOLUME_UP => Some(0x80),
                VK_VOLUME_DOWN => Some(0x81),
                vk => {
                    let mut scan = kb.scanCode as u16;
                    if kb.flags & LLKHF_EXTENDED != 0 {
                        scan |= 0xE000;
                    }
                    keymap::hid_from_win(scan).or_else(|| {
                        let s = MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC_EX) as u16;
                        keymap::hid_from_win(s)
                    })
                }
            };
            let swallow = match hid {
                Some(hid) => dispatch(InputEvent::Key { hid, down }),
                None => is_remote(), // unmapped key: never let it leak locally while remote
            };
            if swallow {
                return 1;
            }
        }
    }
    CallNextHookEx(null_mut(), code, wp, lp)
}

pub fn run_capture(handler: Box<dyn CaptureHandler>) -> Result<(), String> {
    unsafe {
        boost_thread();
        HANDLER.with(|h| *h.borrow_mut() = Some(handler));
        THREAD.store(GetCurrentThreadId(), Ordering::Relaxed);
        let mut p = POINT { x: 0, y: 0 };
        GetCursorPos(&mut p);
        LAST.with(|l| l.set((p.x, p.y)));

        let hinst = GetModuleHandleW(null());
        let mh = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), hinst, 0);
        let kh = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), hinst, 0);
        if mh.is_null() || kh.is_null() {
            return Err(format!("SetWindowsHookEx failed: error {}", GetLastError()));
        }
        SetTimer(null_mut(), 0, 100, None);

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            match msg.message {
                WM_APPLY_GRAB => apply_grab(),
                WM_TIMER => HANDLER.with(|h| {
                    if let Ok(mut h) = h.try_borrow_mut() {
                        if let Some(h) = h.as_mut() {
                            h.tick();
                        }
                    }
                }),
                _ => {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            // tick/event may have queued a grab change.
            apply_grab();
        }
        UnhookWindowsHookEx(mh);
        UnhookWindowsHookEx(kh);
        HANDLER.with(|h| *h.borrow_mut() = None);
        restore_cursor();
    }
    Ok(())
}

pub struct Injector {
    bounds: Rect,
}

impl Injector {
    pub fn new() -> Self {
        Injector { bounds: desktop_bounds() }
    }
}

fn send(inputs: &[INPUT]) {
    unsafe {
        SendInput(inputs.len() as u32, inputs.as_ptr(), size_of::<INPUT>() as i32);
    }
}

fn mouse(dx: i32, dy: i32, data: i32, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

fn kbd(vk: u16, scan: u16, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    }
}

impl RawInjector for Injector {
    fn refresh(&mut self) {
        self.bounds = desktop_bounds();
    }

    fn move_abs(&mut self, x: i32, y: i32) {
        let b = self.bounds;
        let x = x.clamp(b.x, b.right());
        let y = y.clamp(b.y, b.bottom());
        // Normalise to 0..=65535 across the virtual desktop, rounding to nearest pixel centre.
        let nx = (((x - b.x) as i64 * 65535 + (b.w as i64 - 1) / 2) / (b.w as i64 - 1).max(1)) as i32;
        let ny = (((y - b.y) as i64 * 65535 + (b.h as i64 - 1) / 2) / (b.h as i64 - 1).max(1)) as i32;
        send(&[mouse(nx, ny, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK)]);
    }

    fn button(&mut self, button: u8, down: bool) {
        let (flags, data) = match (button, down) {
            (BTN_LEFT, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (BTN_LEFT, false) => (MOUSEEVENTF_LEFTUP, 0),
            (BTN_RIGHT, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (BTN_RIGHT, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (BTN_MIDDLE, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (BTN_MIDDLE, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (BTN_X1, d) => (if d { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, XBUTTON1 as i32),
            (BTN_X2, d) => (if d { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, XBUTTON2 as i32),
            _ => return,
        };
        send(&[mouse(0, 0, data, flags)]);
    }

    fn wheel(&mut self, dx: i32, dy: i32) {
        if dy != 0 {
            send(&[mouse(0, 0, dy, MOUSEEVENTF_WHEEL)]);
        }
        if dx != 0 {
            send(&[mouse(0, 0, dx, MOUSEEVENTF_HWHEEL)]);
        }
    }

    fn key(&mut self, hid: u16, down: bool) {
        let up = if down { 0 } else { KEYEVENTF_KEYUP };
        // Keys whose scan codes don't round-trip through SendInput: use virtual keys.
        let vk = match hid {
            keymap::HID_PAUSE => Some((VK_PAUSE, 0)),
            keymap::HID_NUMLOCK => Some((VK_NUMLOCK, KEYEVENTF_EXTENDEDKEY)),
            0x7F => Some((VK_VOLUME_MUTE, KEYEVENTF_EXTENDEDKEY)),
            0x80 => Some((VK_VOLUME_UP, KEYEVENTF_EXTENDEDKEY)),
            0x81 => Some((VK_VOLUME_DOWN, KEYEVENTF_EXTENDEDKEY)),
            _ => None,
        };
        if let Some((vk, ext)) = vk {
            send(&[kbd(vk, 0, ext | up)]);
            return;
        }
        let Some(scan) = keymap::win_from_hid(hid) else { return };
        let ext = if scan & 0xE000 == 0xE000 { KEYEVENTF_EXTENDEDKEY } else { 0 };
        send(&[kbd(0, scan & 0xFF, KEYEVENTF_SCANCODE | ext | up)]);
    }
}

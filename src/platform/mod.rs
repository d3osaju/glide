//! OS layer. Each backend provides:
//! - `init_process()`         one-time setup (DPI awareness, cursor restore)
//! - `desktop_bounds()`       bounding box of all monitors, in the coordinates the injector uses
//! - `run_capture(handler)`   global input hook loop; the handler may swallow events
//! - `grab(on, home_pos)`     hide/park the local cursor while input goes remote
//! - `restore_cursor()`       undo any cursor hiding (exit / panic path)
//! - `boost_thread()`         raise the current thread's scheduling priority
//! - `Injector`               synthesize input locally

#[derive(Debug, Clone, Copy)]
pub enum InputEvent {
    /// Absolute position (where the cursor would go) and the delta that produced it.
    Motion { x: i32, y: i32, dx: i32, dy: i32 },
    Button { button: u8, down: bool },
    Wheel { dx: i32, dy: i32 },
    Key { hid: u16, down: bool },
}

pub trait CaptureHandler {
    /// Return true to swallow the event (it never reaches the local OS).
    fn event(&mut self, ev: InputEvent) -> bool;
    /// Called roughly every 100 ms on the capture thread.
    fn tick(&mut self);
    /// Whether input is currently routed to a remote screen.
    fn remote(&self) -> bool;
}

/// Low-level injection primitives a backend implements.
pub trait RawInjector {
    fn move_abs(&mut self, x: i32, y: i32);
    fn button(&mut self, button: u8, down: bool);
    fn wheel(&mut self, dx: i32, dy: i32);
    fn key(&mut self, hid: u16, down: bool);
    /// Re-read monitor layout (called when the cursor enters this screen).
    fn refresh(&mut self) {}
}

#[cfg(windows)]
pub mod winaudio;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use self::windows::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use self::macos::*;

#[cfg(not(any(windows, target_os = "macos")))]
mod unsupported;
#[cfg(not(any(windows, target_os = "macos")))]
pub use self::unsupported::*;

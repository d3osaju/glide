use super::{CaptureHandler, RawInjector};
use crate::proto::Rect;

pub fn init_process() {}
pub fn desktop_bounds() -> Rect {
    Rect { x: 0, y: 0, w: 1920, h: 1080 }
}
pub fn boost_thread() {}
pub fn grab(_on: bool, _pos: (i32, i32)) {}
pub fn restore_cursor() {}
pub fn stop_capture() {}
pub fn run_capture(_h: Box<dyn CaptureHandler>) -> Result<(), String> {
    Err("input capture is only implemented for Windows and macOS".into())
}

pub struct Injector;
impl Injector {
    pub fn new() -> Self {
        log::warn!("input injection is not implemented on this OS; events will be dropped");
        Injector
    }
}
impl RawInjector for Injector {
    fn move_abs(&mut self, _x: i32, _y: i32) {}
    fn button(&mut self, _b: u8, _d: bool) {}
    fn wheel(&mut self, _dx: i32, _dy: i32) {}
    fn key(&mut self, _h: u16, _d: bool) {}
}

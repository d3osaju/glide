//! Runtime switches the UI can flip at any moment; checked at the point of use
//! so changes apply instantly without restarting the host or connection.

use std::sync::atomic::{AtomicBool, Ordering};

pub static CLIPBOARD: AtomicBool = AtomicBool::new(true);
/// DSCP priority marking on new sockets.
pub static QOS: AtomicBool = AtomicBool::new(true);
/// 20 ms UDP keepalive from a client while it has the cursor (Wi-Fi power save).
pub static KEEPALIVE: AtomicBool = AtomicBool::new(true);
/// Ctrl+Alt+Esc returns the cursor to the host.
pub static HOTKEY: AtomicBool = AtomicBool::new(true);
/// Host broadcasts its presence on the LAN.
pub static DISCOVERABLE: AtomicBool = AtomicBool::new(true);
/// Don't change screens while a mouse button is held.
pub static BLOCK_DRAG: AtomicBool = AtomicBool::new(true);

pub fn on(f: &AtomicBool) -> bool {
    f.load(Ordering::Relaxed)
}

pub fn set(f: &AtomicBool, v: bool) {
    f.store(v, Ordering::Relaxed)
}

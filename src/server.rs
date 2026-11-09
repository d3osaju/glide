//! Server: owns the physical keyboard and mouse, decides which screen gets
//! them, and streams events to the active client.
//!
//! Latency path for a mouse move: OS hook -> Router::event (in-memory math)
//! -> seal -> UDP send_to, all on the hook thread, no queues or wakeups.
//! Keys, buttons and wheel go over TCP (TCP_NODELAY) through a per-client
//! writer thread so they're never lost or reordered.

use crate::config::{Dir, Layout, ServerConfig};
use crate::crypto::{Key, Opener, Sealer};
use crate::net;
use crate::platform::{self, CaptureHandler, InputEvent};
use crate::proto::{Msg, Rect};
use crate::{clip, flags, keymap};
use std::collections::{HashMap, HashSet};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

pub const PING_EVERY: Duration = Duration::from_secs(2);
pub const DEAD_AFTER: Duration = Duration::from_secs(7);

pub struct Client {
    pub name: String,
    pub screen: Rect,
    id: u64,
    tx: mpsc::Sender<Msg>,
    udp: Mutex<UdpLink>,
    alive: AtomicBool,
    stream: TcpStream,
    pub ip: std::net::IpAddr,
}

struct UdpLink {
    addr: SocketAddr,
    sealer: Sealer,
    ok: bool,
    next_seq: u32,
}

impl Client {
    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    pub fn disconnect(&self) {
        self.alive.store(false, Ordering::Relaxed);
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn udp_ready(&self) -> bool {
        self.udp.lock().unwrap().ok
    }

    /// Reliable, ordered (TCP).
    pub fn send(&self, m: Msg) {
        let _ = self.tx.send(m);
    }

    /// Lowest latency (UDP once the path is confirmed, TCP until then).
    pub fn send_fast(&self, sock: &UdpSocket, m: Msg) {
        let mut link = self.udp.lock().unwrap();
        if link.ok {
            let pkt = link.sealer.seal(&m.to_vec());
            let _ = sock.send_to(&pkt, link.addr);
        } else {
            drop(link);
            self.send(m);
        }
    }

    /// Input that must not be lost (keys, buttons, wheel, enter/leave):
    /// two UDP copies for speed plus one TCP copy as the guaranteed backstop.
    pub fn send_input(&self, sock: &UdpSocket, m: Msg) {
        let mut link = self.udp.lock().unwrap();
        let seq = link.next_seq;
        link.next_seq = link.next_seq.wrapping_add(1);
        let msg = Msg::Seq { seq, inner: Box::new(m) };
        if link.ok {
            let bytes = msg.to_vec();
            for _ in 0..2 {
                let pkt = link.sealer.seal(&bytes);
                let _ = sock.send_to(&pkt, link.addr);
            }
        }
        // Push to TCP while still holding the lock so TCP order == seq order.
        let _ = self.tx.send(msg);
    }
}

#[derive(Default)]
pub struct Registry {
    clients: Mutex<HashMap<String, Arc<Client>>>,
    pub active: Mutex<Option<String>>,
    next_id: AtomicU64,
}

impl Registry {
    pub fn get(&self, name: &str) -> Option<Arc<Client>> {
        self.clients.lock().unwrap().get(name).filter(|c| c.alive()).cloned()
    }

    /// Connected devices.
    pub fn devices(&self) -> Vec<DeviceInfo> {
        let mut v: Vec<_> = self
            .clients
            .lock()
            .unwrap()
            .values()
            .filter(|c| c.alive())
            .map(|c| DeviceInfo { name: c.name.clone(), screen: c.screen, ip: c.ip })
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    pub fn active(&self) -> Option<String> {
        self.active.lock().unwrap().clone()
    }

    fn disconnect_all(&self) {
        for c in self.clients.lock().unwrap().values() {
            c.disconnect();
        }
    }
}

#[derive(Clone)]
pub struct DeviceInfo {
    pub name: String,
    pub screen: Rect,
    pub ip: std::net::IpAddr,
}

pub struct ServerOpts {
    pub listen: String,
    pub psk: Key,
    pub clipboard: bool,
    pub layout: Arc<RwLock<Layout>>,
}

/// A running server. Dropping it does not stop it; call `stop()`.
pub struct ServerHandle {
    pub reg: Arc<Registry>,
    pub port: u16,
    stop: Arc<AtomicBool>,
    capture: Option<std::thread::JoinHandle<Result<(), String>>>,
}

impl ServerHandle {
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Wake the accept loop.
        let _ = TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], self.port)), Duration::from_millis(300));
        self.reg.disconnect_all();
        platform::stop_capture();
        if let Some(t) = self.capture.take() {
            let _ = t.join();
        }
        platform::restore_cursor();
        log::info!("server stopped");
    }

    /// Blocks until the capture loop exits (CLI mode).
    pub fn wait(mut self) -> Result<(), String> {
        match self.capture.take() {
            Some(t) => t.join().unwrap_or_else(|_| Err("capture thread panicked".into())),
            None => Ok(()),
        }
    }
}

pub fn start(opts: ServerOpts) -> Result<ServerHandle, String> {
    let reg = Arc::new(Registry::default());
    let udp = Arc::new(UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("udp bind: {e}"))?);
    net::qos_udp(&udp);
    let listener = TcpListener::bind(&opts.listen).map_err(|e| format!("listen on {}: {e}", opts.listen))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let clip_tx = opts.clipboard.then(|| clip::spawn_server_worker(reg.clone()));
    let stop = Arc::new(AtomicBool::new(false));

    log::info!("server '{}' listening on {}", opts.layout.read().unwrap().me, opts.listen);
    log::info!("local desktop: {:?}", platform::desktop_bounds());
    log::info!("hotkey: Ctrl+Alt+Esc returns the cursor to this machine");

    {
        let (reg, udp, clip_tx, stop, psk) = (reg.clone(), udp.clone(), clip_tx.clone(), stop.clone(), opts.psk);
        std::thread::Builder::new()
            .name("accept".into())
            .spawn(move || accept_loop(listener, psk, reg, udp, clip_tx, stop))
            .unwrap();
    }

    let router = Router::new(opts.layout, reg.clone(), udp, clip_tx);
    let capture = std::thread::Builder::new()
        .name("capture".into())
        .spawn(move || platform::run_capture(Box::new(router)))
        .map_err(|e| e.to_string())?;
    Ok(ServerHandle { reg, port, stop, capture: Some(capture) })
}

/// CLI entry point.
pub fn run(cfg: ServerConfig, psk: Key) -> Result<(), String> {
    flags::set(&flags::BLOCK_DRAG, cfg.block_switch_while_dragging);
    let opts = ServerOpts {
        listen: cfg.listen.clone(),
        psk,
        clipboard: cfg.clipboard,
        layout: Arc::new(RwLock::new(cfg.layout())),
    };
    start(opts)?.wait()
}

fn accept_loop(
    listener: TcpListener,
    psk: Key,
    reg: Arc<Registry>,
    udp: Arc<UdpSocket>,
    clip_tx: Option<mpsc::Sender<clip::Job>>,
    stop: Arc<AtomicBool>,
) {
    for stream in listener.incoming() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let Ok(stream) = stream else { continue };
        let (reg, udp, clip_tx) = (reg.clone(), udp.clone(), clip_tx.clone());
        std::thread::spawn(move || {
            let peer = stream.peer_addr().ok();
            if let Err(e) = handle_client(stream, psk, reg, udp, clip_tx) {
                log::warn!("client {peer:?}: {e}");
            }
        });
    }
}

/// Runs one client connection to completion. Public for the loopback tests.
pub fn handle_client(
    mut stream: TcpStream,
    psk: Key,
    reg: Arc<Registry>,
    udp: Arc<UdpSocket>,
    clip_tx: Option<mpsc::Sender<clip::Job>>,
) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    net::qos_tcp(&stream);
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let keys = net::server_handshake(&mut stream, &psk)?;
    let mut opener = Opener::new(&keys.c2s_tcp, true);
    let (name, screen, udp_port) = match net::read_msg(&mut stream, &mut opener)? {
        Msg::Hello { name, screen, udp_port } => (name, screen, udp_port),
        _ => return Err(net::bad("expected Hello")),
    };
    if screen.w <= 0 || screen.h <= 0 || name.is_empty() {
        return Err(net::bad("invalid Hello"));
    }
    let peer_ip = stream.peer_addr()?.ip();
    stream.set_read_timeout(Some(DEAD_AFTER))?;

    let (tx, rx) = mpsc::channel::<Msg>();
    let client = Arc::new(Client {
        name: name.clone(),
        screen,
        id: reg.next_id.fetch_add(1, Ordering::Relaxed),
        tx,
        udp: Mutex::new(UdpLink {
            addr: SocketAddr::new(peer_ip, udp_port),
            sealer: Sealer::new(&keys.s2c_udp),
            ok: false,
            next_seq: 0,
        }),
        alive: AtomicBool::new(true),
        stream: stream.try_clone()?,
        ip: peer_ip,
    });

    // Writer thread: drains the queue, sends pings and UDP probes when idle.
    let mut wstream = stream.try_clone()?;
    let mut sealer = Sealer::new(&keys.s2c_tcp);
    let wclient = client.clone();
    let wudp = udp.clone();
    let writer = std::thread::spawn(move || {
        platform::boost_thread();
        let mut probe = 0u64;
        let mut send = |m: &Msg| net::write_msg(&mut wstream, &mut sealer, m);
        if send(&Msg::Welcome).is_err() {
            return;
        }
        loop {
            if !wclient.alive() {
                break;
            }
            let res = match rx.recv_timeout(PING_EVERY) {
                Ok(m) => send(&m),
                Err(RecvTimeoutError::Timeout) => send(&Msg::Ping),
                Err(RecvTimeoutError::Disconnected) => break,
            };
            if res.is_err() {
                break;
            }
            let needs_probe = !wclient.udp.lock().unwrap().ok;
            if needs_probe && probe < 30 {
                probe += 1;
                let mut l = wclient.udp.lock().unwrap();
                let pkt = l.sealer.seal(&Msg::UdpProbe(probe).to_vec());
                let _ = wudp.send_to(&pkt, l.addr);
            }
        }
        wclient.alive.store(false, Ordering::Relaxed);
        let _ = wstream.shutdown(std::net::Shutdown::Both);
    });

    // First probe immediately so UDP is usually confirmed within a millisecond.
    {
        let mut l = client.udp.lock().unwrap();
        let pkt = l.sealer.seal(&Msg::UdpProbe(0).to_vec());
        let _ = udp.send_to(&pkt, l.addr);
    }

    if let Some(old) = reg.clients.lock().unwrap().insert(name.clone(), client.clone()) {
        old.disconnect();
    }
    log::info!("client '{name}' connected from {peer_ip}, screen {}x{} at ({},{})", screen.w, screen.h, screen.x, screen.y);

    let result = loop {
        match net::read_msg(&mut stream, &mut opener) {
            Ok(Msg::Ping) => client.send(Msg::Pong),
            Ok(Msg::Pong) => {}
            Ok(Msg::UdpOk(_)) => {
                let mut l = client.udp.lock().unwrap();
                if !l.ok {
                    l.ok = true;
                    log::info!("client '{name}': UDP fast path confirmed");
                }
            }
            Ok(Msg::Clipboard(text)) => {
                if let Some(c) = &clip_tx {
                    let _ = c.send(clip::Job::FromRemote { from: name.clone(), text });
                }
            }
            Ok(other) => log::debug!("client '{name}': ignoring {other:?}"),
            Err(e) => break Err(e),
        }
        if !client.alive() {
            break Ok(());
        }
    };

    client.alive.store(false, Ordering::Relaxed);
    let _ = stream.shutdown(std::net::Shutdown::Both);
    {
        let mut clients = reg.clients.lock().unwrap();
        if clients.get(&name).map(|c| c.id) == Some(client.id) {
            clients.remove(&name);
        }
    }
    let _ = writer.join();
    log::info!("client '{name}' disconnected");
    match result {
        Err(e) if e.kind() != std::io::ErrorKind::UnexpectedEof => Err(e),
        _ => Ok(()),
    }
}

struct Active {
    client: Arc<Client>,
    x: f64,
    y: f64,
    speed: f64,
}

pub struct Router {
    layout: Arc<RwLock<Layout>>,
    reg: Arc<Registry>,
    udp: Arc<UdpSocket>,
    clip_tx: Option<mpsc::Sender<clip::Job>>,
    local: Rect,
    active: Option<Active>,
    /// Every key currently down on the physical keyboard (for the hotkey).
    pressed: HashSet<u16>,
    /// Keys that went down while local; their key-up must reach the local OS.
    held_local: HashSet<u16>,
    buttons_local: HashSet<u8>,
    ticks: u32,
}

impl Router {
    pub fn new(layout: Arc<RwLock<Layout>>, reg: Arc<Registry>, udp: Arc<UdpSocket>, clip_tx: Option<mpsc::Sender<clip::Job>>) -> Self {
        Router {
            layout,
            reg,
            udp,
            clip_tx,
            local: platform::desktop_bounds(),
            active: None,
            pressed: HashSet::new(),
            held_local: HashSet::new(),
            buttons_local: HashSet::new(),
            ticks: 0,
        }
    }

    fn enter(&mut self, client: Arc<Client>, x: f64, y: f64) {
        match self.active.take() {
            None => {
                platform::grab(true, (0, 0));
                if let (Some(c), true) = (&self.clip_tx, flags::on(&flags::CLIPBOARD)) {
                    let _ = c.send(clip::Job::PushTo(client.clone()));
                }
            }
            Some(prev) => prev.client.send_input(&self.udp, Msg::Leave),
        }
        log::info!("-> {}", client.name);
        client.send_input(&self.udp, Msg::Enter { x: x as i32, y: y as i32 });
        *self.reg.active.lock().unwrap() = Some(client.name.clone());
        let speed = self.layout.read().unwrap().speed(&client.name);
        self.active = Some(Active { client, x, y, speed });
    }

    fn go_home(&mut self, pos: Option<(i32, i32)>) {
        if let Some(a) = self.active.take() {
            a.client.send_input(&self.udp, Msg::Leave);
        }
        *self.reg.active.lock().unwrap() = None;
        log::info!("-> {} (local)", self.layout.read().unwrap().me);
        self.local = platform::desktop_bounds();
        platform::grab(false, pos.unwrap_or_else(|| self.local.center()));
    }

    fn ctrl_alt(&self) -> bool {
        let p = &self.pressed;
        (p.contains(&keymap::HID_LCTRL) || p.contains(&keymap::HID_RCTRL))
            && (p.contains(&keymap::HID_LALT) || p.contains(&keymap::HID_RALT))
    }

    fn local_motion(&mut self, x: i32, y: i32, dx: i32, dy: i32) -> bool {
        let layout = self.layout.clone();
        let layout = layout.read().unwrap();
        if flags::on(&flags::BLOCK_DRAG) && !self.buttons_local.is_empty() {
            return false;
        }
        let b = self.local;
        let dir = if x <= b.x && dx <= 0 {
            Dir::Left
        } else if x >= b.right() && dx >= 0 {
            Dir::Right
        } else if y <= b.y && dy <= 0 {
            Dir::Up
        } else if y >= b.bottom() && dy >= 0 {
            Dir::Down
        } else {
            return false;
        };
        if flags::on(&flags::MANUAL_LOCK) || crate::gamelock::blocking() {
            return false; // locked to this screen (Ctrl+Alt+L, or a full-screen game)
        }
        let Some(next) = layout.neighbor(&layout.me, dir).map(str::to_string) else { return false };
        if next == layout.me {
            return false;
        }
        drop(layout); // enter() takes the lock again
        let Some(client) = self.reg.get(&next) else { return false };
        let frac = edge_fraction(dir, x as f64, y as f64, b);
        let (ex, ey) = entry_point(dir, frac, client.screen, 0.0);
        self.enter(client, ex, ey);
        true
    }

    fn remote_motion(&mut self, dx: i32, dy: i32) {
        let me = self.layout.read().unwrap().me.clone();
        let a = self.active.as_mut().unwrap();
        a.x += dx as f64 * a.speed;
        a.y += dy as f64 * a.speed;
        let r = a.client.screen;
        let dir = if a.x < r.x as f64 {
            Some(Dir::Left)
        } else if a.x > r.right() as f64 {
            Some(Dir::Right)
        } else if a.y < r.y as f64 {
            Some(Dir::Up)
        } else if a.y > r.bottom() as f64 {
            Some(Dir::Down)
        } else {
            None
        };
        let dir = dir.filter(|_| !flags::on(&flags::MANUAL_LOCK));
        if dir.is_none() {
            a.x = a.x.clamp(r.x as f64, r.right() as f64);
            a.y = a.y.clamp(r.y as f64, r.bottom() as f64);
        }
        if let Some(dir) = dir {
            let frac = edge_fraction(dir, a.x, a.y, r);
            let next = self.layout.read().unwrap().neighbor(&a.client.name, dir).map(str::to_string);
            match next {
                Some(n) if n == me => {
                    let (hx, hy) = entry_point(dir, frac, self.local, 2.0);
                    self.go_home(Some((hx as i32, hy as i32)));
                    return;
                }
                Some(n) => {
                    if let Some(c) = self.reg.get(&n) {
                        let (ex, ey) = entry_point(dir, frac, c.screen, 0.0);
                        self.enter(c, ex, ey);
                        return;
                    }
                }
                None => {}
            }
            a.x = a.x.clamp(r.x as f64, r.right() as f64);
            a.y = a.y.clamp(r.y as f64, r.bottom() as f64);
        }
        a.client.send_fast(&self.udp, Msg::Move { x: a.x.round() as i32, y: a.y.round() as i32 });
    }
}

/// Position along the crossed edge, 0.0..=1.0.
fn edge_fraction(dir: Dir, x: f64, y: f64, r: Rect) -> f64 {
    let f = match dir {
        Dir::Left | Dir::Right => (y - r.y as f64) / (r.h - 1).max(1) as f64,
        Dir::Up | Dir::Down => (x - r.x as f64) / (r.w - 1).max(1) as f64,
    };
    f.clamp(0.0, 1.0)
}

/// Where the cursor appears on screen `r` after travelling in `dir`, `inset`
/// pixels inside the entered edge.
fn entry_point(dir: Dir, frac: f64, r: Rect, inset: f64) -> (f64, f64) {
    let along_y = r.y as f64 + frac * (r.h - 1) as f64;
    let along_x = r.x as f64 + frac * (r.w - 1) as f64;
    match dir {
        Dir::Right => (r.x as f64 + inset, along_y),
        Dir::Left => (r.right() as f64 - inset, along_y),
        Dir::Down => (along_x, r.y as f64 + inset),
        Dir::Up => (along_x, r.bottom() as f64 - inset),
    }
}

impl CaptureHandler for Router {
    fn event(&mut self, ev: InputEvent) -> bool {
        match ev {
            InputEvent::Key { hid, down } => {
                if down {
                    self.pressed.insert(hid);
                } else {
                    self.pressed.remove(&hid);
                }
                if down && hid == keymap::HID_L && self.ctrl_alt() {
                    let on = !flags::on(&flags::MANUAL_LOCK);
                    flags::set(&flags::MANUAL_LOCK, on);
                    log::info!("screen lock {}", if on { "on" } else { "off" });
                    return true;
                }
                if !down && hid == keymap::HID_L && self.ctrl_alt() {
                    return true;
                }
                if self.active.is_none() {
                    if down {
                        self.held_local.insert(hid);
                    } else {
                        self.held_local.remove(&hid);
                    }
                    return false;
                }
                if down && hid == keymap::HID_ESC && flags::on(&flags::HOTKEY) && self.ctrl_alt() {
                    self.go_home(None);
                    return true;
                }
                if !down && self.held_local.remove(&hid) {
                    return false; // pressed before we switched away: release it locally
                }
                self.active.as_ref().unwrap().client.send_input(&self.udp, Msg::Key { hid, down });
                true
            }
            InputEvent::Motion { x, y, dx, dy } => {
                if self.active.is_none() {
                    self.local_motion(x, y, dx, dy)
                } else {
                    if dx != 0 || dy != 0 {
                        self.remote_motion(dx, dy);
                    }
                    true
                }
            }
            InputEvent::Button { button, down } => match &self.active {
                None => {
                    if down {
                        self.buttons_local.insert(button);
                    } else {
                        self.buttons_local.remove(&button);
                    }
                    false
                }
                Some(a) => {
                    if !down && self.buttons_local.remove(&button) {
                        return false;
                    }
                    a.client.send_input(&self.udp, Msg::Button { button, down });
                    true
                }
            },
            InputEvent::Wheel { dx, dy } => match &self.active {
                None => false,
                Some(a) => {
                    a.client.send_input(&self.udp, Msg::Wheel { dx, dy });
                    true
                }
            },
        }
    }

    fn tick(&mut self) {
        self.ticks = self.ticks.wrapping_add(1);
        if let Some(a) = &self.active {
            if !a.client.alive() {
                log::warn!("active client '{}' dropped; returning home", a.client.name);
                self.go_home(None);
            }
        } else if self.ticks % 20 == 0 {
            // Pick up monitor layout changes.
            self.local = platform::desktop_bounds();
        }
    }

    fn remote(&self) -> bool {
        self.active.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_maps_proportionally() {
        let a = Rect { x: 0, y: 0, w: 1920, h: 1080 };
        let b = Rect { x: 0, y: 0, w: 1440, h: 900 };
        let f = edge_fraction(Dir::Right, 1919.0, 540.0, a);
        let (x, y) = entry_point(Dir::Right, f, b, 0.0);
        assert_eq!(x, 0.0);
        assert!((y - 449.5).abs() < 1.0);
        let (hx, _) = entry_point(Dir::Left, 0.5, a, 2.0);
        assert_eq!(hx, 1917.0);
    }
}

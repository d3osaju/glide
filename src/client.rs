//! Client: receives events from the server and injects them locally.
//!
//! - Mouse moves arrive on a high-priority UDP thread that injects directly.
//! - Keys/buttons/wheel/enter/leave arrive as sequenced events over UDP (two
//!   copies) and TCP (one copy). `Output::deliver` applies each sequence
//!   number exactly once and in order, from whichever copy lands first.
//! - While the cursor is on this screen, a tiny UDP keepalive goes out every
//!   20 ms so Wi-Fi power-saving never parks the radio between packets
//!   (the classic cause of "laggy on Wi-Fi" in KVM software).

use crate::clip;
use crate::crypto::{Key, Opener, Sealer};
use crate::net;
use crate::platform::{self, RawInjector};
use crate::proto::{Msg, Rect};
use crate::{keymap, server};
use std::collections::{BTreeMap, HashSet};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct Options {
    /// Address from the connection code. Hosts on DHCP change address after a
    /// restart or Wi-Fi reconnect, so `find` is tried first when available.
    pub server: String,
    pub name: String,
    pub key: Key,
    pub swap_ctrl_cmd: bool,
    pub clipboard: bool,
    /// Current address of this host as seen by LAN discovery (matched by key fingerprint).
    pub find: Option<Arc<dyn Fn() -> Vec<SocketAddr> + Send + Sync>>,
}

/// Wraps a backend injector. Tracks what is held so a disconnect or screen
/// switch can never leave a key or button stuck, and reorders sequenced input.
pub struct Output<I: RawInjector> {
    pub inj: I,
    keys: HashSet<u16>,
    buttons: HashSet<u8>,
    swap: bool,
    next_seq: u32,
    pending: BTreeMap<u32, Msg>,
    pub entered: bool,
}

impl<I: RawInjector> Output<I> {
    pub fn new(inj: I, swap: bool) -> Self {
        Output {
            inj,
            keys: HashSet::new(),
            buttons: HashSet::new(),
            swap,
            next_seq: 0,
            pending: BTreeMap::new(),
            entered: false,
        }
    }

    pub fn new_session(&mut self) {
        self.release_all();
        self.next_seq = 0;
        self.pending.clear();
        self.entered = false;
    }

    /// Apply sequenced input exactly once, in order.
    pub fn deliver(&mut self, seq: u32, m: Msg) {
        if seq.wrapping_sub(self.next_seq) >= u32::MAX / 2 || self.pending.contains_key(&seq) {
            return; // already applied (duplicate copy)
        }
        self.pending.insert(seq, m);
        while let Some(m) = self.pending.remove(&self.next_seq) {
            self.next_seq = self.next_seq.wrapping_add(1);
            self.apply(&m);
        }
    }

    pub fn apply(&mut self, m: &Msg) {
        match *m {
            Msg::Move { x, y } => self.inj.move_abs(x, y),
            Msg::Enter { x, y } => {
                self.entered = true;
                self.inj.refresh();
                self.inj.move_abs(x, y);
            }
            Msg::Button { button, down } => {
                if down {
                    self.buttons.insert(button);
                } else if !self.buttons.remove(&button) {
                    return;
                }
                self.inj.button(button, down);
            }
            Msg::Wheel { dx, dy } => self.inj.wheel(dx, dy),
            Msg::Key { hid, down } => {
                let hid = if self.swap { keymap::swap_ctrl_gui(hid) } else { hid };
                if down {
                    self.keys.insert(hid);
                } else if !self.keys.remove(&hid) {
                    return;
                }
                self.inj.key(hid, down);
            }
            Msg::Leave => {
                self.entered = false;
                self.release_all();
            }
            _ => {}
        }
    }

    pub fn release_all(&mut self) {
        for hid in std::mem::take(&mut self.keys) {
            self.inj.key(hid, false);
        }
        for b in std::mem::take(&mut self.buttons) {
            self.inj.button(b, false);
        }
    }
}

pub enum ClipEvent<'a> {
    /// The cursor left this screen: send our clipboard if it changed.
    Leave(&'a mut dyn FnMut(Msg)),
    /// The server sent clipboard text.
    Set(String),
}

/// Connection state shared with the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Connecting,
    Connected,
    Error(String),
    Stopped,
}

/// Lets the UI stop a running client and observe its status.
pub struct Ctl {
    stop: AtomicBool,
    status: Mutex<Status>,
    stream: Mutex<Option<TcpStream>>,
    /// True while the cursor is on this machine.
    pub active: AtomicBool,
    /// Address of the host we're connected to (may differ from the code's).
    addr: Mutex<Option<SocketAddr>>,
}

impl Ctl {
    pub fn new() -> Arc<Self> {
        Arc::new(Ctl {
            stop: AtomicBool::new(false),
            status: Mutex::new(Status::Connecting),
            stream: Mutex::new(None),
            active: AtomicBool::new(false),
            addr: Mutex::new(None),
        })
    }
    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }
    fn set(&self, s: Status) {
        *self.status.lock().unwrap() = s;
    }
    pub fn addr(&self) -> Option<SocketAddr> {
        *self.addr.lock().unwrap()
    }
    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(s) = self.stream.lock().unwrap().take() {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }
}

/// Connects and reconnects until `ctl.stop()` is called.
pub fn run(opts: Options, ctl: Arc<Ctl>) {
    let out = Arc::new(Mutex::new(Output::new(platform::Injector::new(), opts.swap_ctrl_cmd)));
    let mut cb = None;
    let mut clip_seen: Option<u64> = None;
    let clipboard = opts.clipboard;
    let mut on_clip = |ev: ClipEvent| {
        if !clipboard || !crate::flags::on(&crate::flags::CLIPBOARD) {
            return;
        }
        match ev {
            ClipEvent::Leave(send) => {
                if let Some(text) = clip::read_text(&mut cb) {
                    let d = clip::digest(&text);
                    if clip_seen != Some(d) {
                        clip_seen = Some(d);
                        send(Msg::Clipboard(text));
                    }
                }
            }
            ClipEvent::Set(text) => {
                clip_seen = Some(clip::digest(&text));
                clip::write_text(&mut cb, &text);
            }
        }
    };
    while !ctl.stopped() {
        ctl.set(Status::Connecting);
        let res = session(&opts, out.clone(), platform::desktop_bounds(), &ctl, &mut on_clip);
        out.lock().unwrap().new_session();
        ctl.active.store(false, Ordering::Relaxed);
        if ctl.stopped() {
            break;
        }
        match res {
            Ok(()) => {
                log::info!("disconnected from server");
                ctl.set(Status::Error("Host closed the connection".into()));
            }
            Err(e) => {
                log::warn!("{}: {e}", opts.server);
                ctl.set(Status::Error(friendly(&e)));
            }
        }
        for _ in 0..20 {
            if ctl.stopped() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    ctl.set(Status::Stopped);
}

fn friendly(e: &std::io::Error) -> String {
    use std::io::ErrorKind::*;
    match e.kind() {
        ConnectionRefused => "Host isn't running glide (connection refused)".into(),
        TimedOut | WouldBlock => "Host not reachable: check the address and that both are on the same network".into(),
        _ if e.to_string().contains("wrong key") => "Wrong key: copy the connection code from the host again".into(),
        _ => e.to_string(),
    }
}

/// One connection. Generic over the injector so the loopback test can observe output.
pub fn session<I: RawInjector + Send + 'static>(
    opts: &Options,
    out: Arc<Mutex<Output<I>>>,
    screen: Rect,
    ctl: &Ctl,
    on_clip: &mut dyn FnMut(ClipEvent),
) -> std::io::Result<()> {
    // Where the host is now (discovery) first, then the address in the code.
    let coded = opts.server.to_socket_addrs().ok().and_then(|mut a| a.next());
    let mut candidates: Vec<SocketAddr> = opts.find.as_ref().map(|f| f()).unwrap_or_default();
    if let Some(c) = coded {
        if !candidates.contains(&c) {
            candidates.push(c);
        }
    }
    if candidates.is_empty() {
        return Err(net::bad("could not resolve server address"));
    }
    let mut connected = None;
    let mut last_err = None;
    for a in candidates {
        match TcpStream::connect_timeout(&a, Duration::from_secs(3)) {
            Ok(s) => {
                connected = Some((s, a));
                break;
            }
            Err(e) => last_err = Some(e),
        }
    }
    let Some((mut stream, addr)) = connected else {
        return Err(last_err.unwrap_or_else(|| net::bad("host unreachable")));
    };
    if Some(addr) != coded {
        log::info!("host found at {addr} (the code said {})", opts.server);
    }
    *ctl.addr.lock().unwrap() = Some(addr);
    stream.set_nodelay(true)?;
    net::qos_tcp(&stream);
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    *ctl.stream.lock().unwrap() = Some(stream.try_clone()?);
    if ctl.stopped() {
        return Ok(());
    }
    let keys = net::client_handshake(&mut stream, &opts.key)?;

    let udp = UdpSocket::bind(("0.0.0.0", 0))?;
    net::qos_udp(&udp);
    udp.set_read_timeout(Some(Duration::from_millis(20)))?;
    let udp_port = udp.local_addr()?.port();

    let writer = Arc::new(Mutex::new((stream.try_clone()?, Sealer::new(&keys.c2s_tcp))));
    let send = {
        let writer = writer.clone();
        move |m: Msg| {
            let mut w = writer.lock().unwrap();
            let (s, sealer) = &mut *w;
            let _ = net::write_msg(s, sealer, &m);
        }
    };
    send(Msg::Hello { name: opts.name.clone(), screen, udp_port });

    let mut opener = Opener::new(&keys.s2c_tcp, true);
    match net::read_msg(&mut stream, &mut opener) {
        Ok(Msg::Welcome) => {}
        Ok(_) => return Err(net::bad("unexpected reply to Hello")),
        Err(e) => return Err(net::bad(&format!("handshake failed (wrong key?): {e}"))),
    }
    log::info!("connected to {} as '{}' (screen {}x{} at {},{})", opts.server, opts.name, screen.w, screen.h, screen.x, screen.y);
    stream.set_read_timeout(Some(server::DEAD_AFTER))?;
    out.lock().unwrap().new_session();
    ctl.set(Status::Connected);

    let stop = Arc::new(AtomicBool::new(false));

    // UDP fast path + Wi-Fi keepalive.
    let udp_thread = {
        let (stop, out, send) = (stop.clone(), out.clone(), send.clone());
        let server_ip = addr.ip();
        let mut uopen = Opener::new(&keys.s2c_udp, false);
        std::thread::Builder::new().name("udp-rx".into()).spawn(move || {
            platform::boost_thread();
            let mut buf = [0u8; 1500];
            let mut server_udp: Option<SocketAddr> = None;
            let mut last_ka = Instant::now();
            while !stop.load(Ordering::Relaxed) {
                if let Ok((n, from)) = udp.recv_from(&mut buf) {
                    if from.ip() == server_ip {
                        if let Some(m) = uopen.open(&buf[..n]).and_then(|pt| Msg::decode(&pt)) {
                            server_udp = Some(from);
                            match m {
                                Msg::Move { .. } => out.lock().unwrap().apply(&m),
                                Msg::Seq { seq, inner } => out.lock().unwrap().deliver(seq, *inner),
                                Msg::UdpProbe(p) => send(Msg::UdpOk(p)),
                                _ => {}
                            }
                        }
                    }
                }
                if let Some(to) = server_udp {
                    if last_ka.elapsed() >= Duration::from_millis(20)
                        && crate::flags::on(&crate::flags::KEEPALIVE)
                        && out.lock().unwrap().entered
                    {
                        last_ka = Instant::now();
                        let _ = udp.send_to(&[0u8], to);
                    }
                }
            }
        })?
    };

    // Keepalive so the server notices a dead client even while it's busy streaming.
    let ping_thread = {
        let (stop, send) = (stop.clone(), send.clone());
        std::thread::spawn(move || {
            let mut slept = 0;
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
                slept += 100;
                if slept >= 2000 {
                    slept = 0;
                    send(Msg::Ping);
                }
            }
        })
    };

    platform::boost_thread();
    let mut send_mut = send.clone();
    let result = loop {
        let m = match net::read_msg(&mut stream, &mut opener) {
            Ok(m) => m,
            Err(e) => break Err(e),
        };
        match m {
            Msg::Ping => send(Msg::Pong),
            Msg::Pong => {}
            Msg::Clipboard(text) => on_clip(ClipEvent::Set(text)),
            Msg::Seq { seq, inner } => {
                let is_leave = matches!(*inner, Msg::Leave);
                let entered = {
                    let mut o = out.lock().unwrap();
                    o.deliver(seq, *inner);
                    o.entered
                };
                ctl.active.store(entered, Ordering::Relaxed);
                if is_leave {
                    on_clip(ClipEvent::Leave(&mut send_mut));
                }
            }
            m @ Msg::Move { .. } => out.lock().unwrap().apply(&m),
            other => log::debug!("ignoring {other:?}"),
        }
    };

    stop.store(true, Ordering::Relaxed);
    let _ = stream.shutdown(std::net::Shutdown::Both);
    let _ = udp_thread.join();
    let _ = ping_thread.join();
    match result {
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(()),
        r => r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Log(Vec<(u16, bool)>);
    impl RawInjector for Log {
        fn move_abs(&mut self, _: i32, _: i32) {}
        fn button(&mut self, _: u8, _: bool) {}
        fn wheel(&mut self, _: i32, _: i32) {}
        fn key(&mut self, hid: u16, down: bool) {
            self.0.push((hid, down));
        }
    }

    #[test]
    fn deliver_reorders_and_dedups() {
        let mut o = Output::new(Log::default(), false);
        let k = |h, d| Msg::Key { hid: h, down: d };
        o.deliver(1, k(5, true)); // arrives early: held back
        o.deliver(0, k(4, true));
        o.deliver(0, k(4, true)); // duplicate copy
        o.deliver(1, k(5, true)); // duplicate copy
        o.deliver(3, k(5, false));
        o.deliver(2, k(4, false));
        assert_eq!(o.inj.0, vec![(4, true), (5, true), (4, false), (5, false)]);
    }
}

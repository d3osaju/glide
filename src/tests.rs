//! End-to-end loopback test: a real server connection handler and a real
//! client session talking over 127.0.0.1 (encrypted TCP + UDP), with a
//! recording injector in place of the OS. Measures event latency.

use crate::client::{self, Output};
use crate::platform::RawInjector;
use crate::proto::{Msg, Rect};
use crate::server::{self, Registry};
use std::net::{TcpListener, UdpSocket};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Rec(mpsc::Sender<(Msg, Instant)>);

impl RawInjector for Rec {
    fn move_abs(&mut self, x: i32, y: i32) {
        let _ = self.0.send((Msg::Move { x, y }, Instant::now()));
    }
    fn button(&mut self, button: u8, down: bool) {
        let _ = self.0.send((Msg::Button { button, down }, Instant::now()));
    }
    fn wheel(&mut self, dx: i32, dy: i32) {
        let _ = self.0.send((Msg::Wheel { dx, dy }, Instant::now()));
    }
    fn key(&mut self, hid: u16, down: bool) {
        let _ = self.0.send((Msg::Key { hid, down }, Instant::now()));
    }
}

fn pct(v: &mut [Duration], p: f64) -> Duration {
    v.sort();
    v[((v.len() - 1) as f64 * p) as usize]
}

#[test]
fn loopback_end_to_end() {
    let psk = [9u8; 32];
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let reg = Arc::new(Registry::default());
    let udp = Arc::new(UdpSocket::bind("127.0.0.1:0").unwrap());
    {
        let (reg, udp) = (reg.clone(), udp.clone());
        std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let _ = server::handle_client(s, psk, reg, udp, None);
        });
    }

    let (tx, rx) = mpsc::channel();
    let out = Arc::new(Mutex::new(Output::new(Rec(tx), false)));
    std::thread::spawn(move || {
        let opts = client::Options {
            server: addr.to_string(),
            name: "laptop".into(),
            key: psk,
            swap_ctrl_cmd: false,
            clipboard: false,
            find: None,
        };
        let screen = Rect { x: 0, y: 0, w: 2560, h: 1600 };
        let ctl = client::Ctl::new();
        let _ = client::session(&opts, out, screen, &ctl, &mut |_| {});
    });

    // Wait for the connection and the UDP fast path.
    let deadline = Instant::now() + Duration::from_secs(5);
    let c = loop {
        if let Some(c) = reg.get("laptop").filter(|c| c.udp_ready()) {
            break c;
        }
        assert!(Instant::now() < deadline, "client never connected / UDP never confirmed");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(c.screen, Rect { x: 0, y: 0, w: 2560, h: 1600 });

    // Mouse moves over UDP.
    let mut udp_lat = Vec::new();
    for i in 0..2000 {
        let t0 = Instant::now();
        c.send_fast(&udp, Msg::Move { x: i, y: i / 2 });
        let (m, t1) = rx.recv_timeout(Duration::from_secs(1)).expect("move lost on loopback");
        assert_eq!(m, Msg::Move { x: i, y: i / 2 });
        udp_lat.push(t1 - t0);
    }

    // Keys: sequenced, 2x UDP + TCP backstop, with the stuck-key safety net on Leave.
    let mut tcp_lat = Vec::new();
    for i in 0..500 {
        let down = i % 2 == 0;
        let t0 = Instant::now();
        c.send_input(&udp, Msg::Key { hid: 0x04, down });
        let (m, t1) = rx.recv_timeout(Duration::from_secs(1)).expect("key lost");
        assert_eq!(m, Msg::Key { hid: 0x04, down });
        tcp_lat.push(t1 - t0);
    }
    c.send_input(&udp, Msg::Key { hid: 0xE1, down: true });
    c.send_input(&udp, Msg::Button { button: 1, down: true });
    c.send_input(&udp, Msg::Leave);
    let got: Vec<Msg> = (0..4).map(|_| rx.recv_timeout(Duration::from_secs(1)).unwrap().0).collect();
    assert!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "an input event was applied twice");
    assert!(got.contains(&Msg::Key { hid: 0xE1, down: false }), "Leave must release held keys: {got:?}");
    assert!(got.contains(&Msg::Button { button: 1, down: false }), "Leave must release held buttons: {got:?}");

    let (u50, u99) = (pct(&mut udp_lat, 0.5), pct(&mut udp_lat, 0.99));
    let (t50, t99) = (pct(&mut tcp_lat, 0.5), pct(&mut tcp_lat, 0.99));
    println!("mouse/UDP latency p50 {u50:?} p99 {u99:?}");
    println!("key/seq   latency p50 {t50:?} p99 {t99:?}");
    assert!(u99 < Duration::from_millis(5), "UDP p99 too high: {u99:?}");
    assert!(t99 < Duration::from_millis(5), "key p99 too high: {t99:?}");
}

/// Nothing else may arrive: every sequenced event was applied exactly once
/// even though each was sent three times.
#[test]
fn no_duplicate_application() {
    // Covered by client::tests::deliver_reorders_and_dedups at unit level;
    // here we only make sure the wire format of Seq survives sealing.
    let k = crate::crypto::derive(&[3; 32], "x", b"");
    let mut s = crate::crypto::Sealer::new(&k);
    let mut o = crate::crypto::Opener::new(&k, false);
    let m = Msg::Seq { seq: 9, inner: Box::new(Msg::Leave) };
    let pt = o.open(&s.seal(&m.to_vec())).unwrap();
    assert_eq!(Msg::decode(&pt), Some(m));
}

/// The host got a new DHCP address: the code's address is dead, but LAN
/// discovery knows where the host is now. The client must connect anyway.
#[test]
fn follows_host_to_new_address() {
    let psk = [4u8; 32];
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let real = listener.local_addr().unwrap();
    let reg = Arc::new(Registry::default());
    let udp = Arc::new(UdpSocket::bind("127.0.0.1:0").unwrap());
    {
        let (reg, udp) = (reg.clone(), udp.clone());
        std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let _ = server::handle_client(s, psk, reg, udp, None);
        });
    }
    // Nothing listens here: the "old" address from the connection code.
    let dead = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let (tx, _rx) = mpsc::channel();
    let out = Arc::new(Mutex::new(Output::new(Rec(tx), false)));
    let ctl = client::Ctl::new();
    {
        let ctl = ctl.clone();
        std::thread::spawn(move || {
            let opts = client::Options {
                server: dead.to_string(),
                name: "moved".into(),
                key: psk,
                swap_ctrl_cmd: false,
                clipboard: false,
                find: Some(Arc::new(move || vec![dead, real])), // unreachable adapter first
            };
            let _ = client::session(&opts, out, Rect { x: 0, y: 0, w: 800, h: 600 }, &ctl, &mut |_| {});
        });
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while reg.get("moved").is_none() {
        assert!(Instant::now() < deadline, "client never reached the host at its new address");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(ctl.addr(), Some(real));
}

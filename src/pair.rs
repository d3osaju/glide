//! Pairing helpers for the GUI:
//! - Connection codes: one copy-pasteable string carrying host IP, port and key.
//! - LAN discovery: the host broadcasts a small beacon (name + port, *no key*)
//!   so clients can list hosts on the network without typing an address.

use crate::crypto::{self, Key};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const DISCOVERY_PORT: u16 = 24801;
const BEACON: &[u8; 5] = b"GLDB1";
const CODE_PREFIX: &str = "glide-";

pub fn encode_code(ip: Ipv4Addr, port: u16, key: &Key) -> String {
    let mut b = Vec::with_capacity(38);
    b.extend_from_slice(&ip.octets());
    b.extend_from_slice(&port.to_be_bytes());
    b.extend_from_slice(key);
    format!("{CODE_PREFIX}{}", URL_SAFE_NO_PAD.encode(b))
}

pub fn decode_code(code: &str) -> Result<(SocketAddr, Key), String> {
    let c = code.trim();
    let c = c.strip_prefix(CODE_PREFIX).unwrap_or(c);
    let b = URL_SAFE_NO_PAD.decode(c).map_err(|_| "That doesn't look like a glide connection code".to_string())?;
    if b.len() != 38 {
        return Err("Connection code is incomplete. Copy it again from the host".into());
    }
    let ip = Ipv4Addr::new(b[0], b[1], b[2], b[3]);
    let port = u16::from_be_bytes([b[4], b[5]]);
    let key: Key = b[6..].try_into().unwrap();
    Ok((SocketAddr::new(IpAddr::V4(ip), port), key))
}

/// Short, non-secret fingerprint of a key, for showing "same key" in the UI.
pub fn fingerprint(key: &Key) -> String {
    let d = crypto::derive(key, "glide v1 fingerprint", b"");
    d[..3].iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join("-")
}

#[derive(Clone, serde::Serialize)]
pub struct LocalIp {
    pub ip: String,
    pub iface: String,
}

/// LAN IPv4 addresses, the default-route one first.
pub fn local_ips() -> Vec<LocalIp> {
    let primary = primary_ip();
    let mut v: Vec<LocalIp> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|i| match i.ip() {
            IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_link_local() => {
                Some(LocalIp { ip: ip.to_string(), iface: i.name.clone() })
            }
            _ => None,
        })
        .collect();
    v.sort_by_key(|l| Some(l.ip.as_str()) != primary.map(|p| p.to_string()).as_deref());
    v.dedup_by(|a, b| a.ip == b.ip);
    v
}

/// The address the OS would use to reach the internet (no packet is sent).
pub fn primary_ip() -> Option<Ipv4Addr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:80").ok()?;
    match s.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

fn broadcast_targets() -> Vec<SocketAddr> {
    let mut v: Vec<SocketAddr> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|i| match i.addr {
            if_addrs::IfAddr::V4(a) if !a.ip.is_loopback() => a.broadcast.map(|b| SocketAddr::new(IpAddr::V4(b), DISCOVERY_PORT)),
            _ => None,
        })
        .collect();
    v.push(SocketAddr::new(IpAddr::V4(Ipv4Addr::BROADCAST), DISCOVERY_PORT));
    v.dedup();
    v
}

/// Broadcast "I'm a glide host" every 1.5 s until `stop` is set.
pub fn spawn_beacon(name: String, port: u16, key_fp: String, stop: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else { return };
        let _ = sock.set_broadcast(true);
        let mut pkt = BEACON.to_vec();
        pkt.extend_from_slice(&port.to_be_bytes());
        pkt.push(key_fp.len() as u8);
        pkt.extend_from_slice(key_fp.as_bytes());
        pkt.extend_from_slice(name.as_bytes());
        let mut targets = broadcast_targets();
        let mut n = 0u32;
        while !stop.load(Ordering::Relaxed) {
            if n % 20 == 0 {
                targets = broadcast_targets(); // network changes (Wi-Fi switch etc.)
            }
            n += 1;
            if crate::flags::on(&crate::flags::DISCOVERABLE) {
                for t in &targets {
                    let _ = sock.send_to(&pkt, t);
                }
            }
            for _ in 0..15 {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    });
}

#[derive(Clone, serde::Serialize)]
pub struct FoundHost {
    pub name: String,
    pub addr: String,
    pub key_fp: String,
}

/// Collects beacons. Hosts not heard from for 5 s drop off the list.
#[derive(Default)]
pub struct Discovery {
    hosts: Mutex<HashMap<SocketAddr, (FoundHost, Instant)>>,
    started: AtomicBool,
}

impl Discovery {
    pub fn start(self: &Arc<Self>) {
        if self.started.swap(true, Ordering::Relaxed) {
            return;
        }
        let me = self.clone();
        std::thread::spawn(move || {
            let sock = match bind_reuse(DISCOVERY_PORT) {
                Ok(s) => s,
                Err(e) => {
                    log::warn!("discovery unavailable: {e}");
                    return;
                }
            };
            let mut buf = [0u8; 512];
            loop {
                let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
                let p = &buf[..n];
                if n < 8 || &p[..5] != BEACON {
                    continue;
                }
                let port = u16::from_be_bytes([p[5], p[6]]);
                let fl = p[7] as usize;
                if n < 8 + fl {
                    continue;
                }
                let key_fp = String::from_utf8_lossy(&p[8..8 + fl]).to_string();
                let name = String::from_utf8_lossy(&p[8 + fl..]).chars().take(64).collect::<String>();
                let addr = SocketAddr::new(from.ip(), port);
                me.hosts
                    .lock()
                    .unwrap()
                    .insert(addr, (FoundHost { name, addr: addr.to_string(), key_fp }, Instant::now()));
            }
        });
    }

    pub fn list(&self) -> Vec<FoundHost> {
        let mut h = self.hosts.lock().unwrap();
        h.retain(|_, (_, seen)| seen.elapsed() < Duration::from_secs(5));
        let mut v: Vec<_> = h.values().map(|(f, _)| f.clone()).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }
}

fn bind_reuse(port: u16) -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    s.bind(&SocketAddr::from(([0, 0, 0, 0], port)).into())?;
    Ok(s.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_roundtrip() {
        let key = [7u8; 32];
        let code = encode_code(Ipv4Addr::new(192, 168, 1, 10), 24800, &key);
        assert!(code.starts_with("glide-"));
        let (addr, k) = decode_code(&format!("  {code}\n")).unwrap();
        assert_eq!(addr.to_string(), "192.168.1.10:24800");
        assert_eq!(k, key);
        assert!(decode_code("glide-abc").is_err());
        assert_eq!(fingerprint(&key).len(), 8);
    }
}

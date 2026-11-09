//! TCP framing and the session handshake.
//!
//! Handshake (cleartext): client sends `GLD1 || client_nonce[32]`, server
//! answers `GLD1 || server_nonce[32]`. Both derive the session keys from the
//! PSK and the two nonces. The first sealed frame (Hello / Welcome) proves
//! both sides hold the same key; a wrong key fails decryption there.

use crate::crypto::{self, Key, Opener, Sealer, SessionKeys};
use crate::proto::Msg;
use std::io::{self, Read, Write};
use std::net::TcpStream;

const MAGIC: &[u8; 4] = b"GLD1";
const MAX_FRAME: usize = crate::proto::MAX_CLIPBOARD + 1024;

pub fn client_handshake(s: &mut TcpStream, psk: &Key) -> io::Result<SessionKeys> {
    let cn: [u8; 32] = crypto::random();
    let mut hello = Vec::with_capacity(36);
    hello.extend_from_slice(MAGIC);
    hello.extend_from_slice(&cn);
    s.write_all(&hello)?;
    let mut resp = [0u8; 36];
    s.read_exact(&mut resp)?;
    if &resp[..4] != MAGIC {
        return Err(bad("not a glide server"));
    }
    let sn: [u8; 32] = resp[4..].try_into().unwrap();
    Ok(SessionKeys::derive(psk, &cn, &sn))
}

pub fn server_handshake(s: &mut TcpStream, psk: &Key) -> io::Result<SessionKeys> {
    let mut req = [0u8; 36];
    s.read_exact(&mut req)?;
    if &req[..4] != MAGIC {
        return Err(bad("not a glide client"));
    }
    let cn: [u8; 32] = req[4..].try_into().unwrap();
    let sn: [u8; 32] = crypto::random();
    let mut resp = Vec::with_capacity(36);
    resp.extend_from_slice(MAGIC);
    resp.extend_from_slice(&sn);
    s.write_all(&resp)?;
    Ok(SessionKeys::derive(psk, &cn, &sn))
}

/// Writes one sealed frame in a single write call (one TCP segment for small
/// messages, which matters with TCP_NODELAY).
pub fn write_msg(w: &mut impl Write, sealer: &mut Sealer, msg: &Msg) -> io::Result<()> {
    let sealed = sealer.seal(&msg.to_vec());
    let mut frame = Vec::with_capacity(4 + sealed.len());
    frame.extend_from_slice(&(sealed.len() as u32).to_le_bytes());
    frame.extend_from_slice(&sealed);
    w.write_all(&frame)
}

pub fn read_msg(r: &mut impl Read, opener: &mut Opener) -> io::Result<Msg> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(bad("frame too large"));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    let pt = opener.open(&buf).ok_or_else(|| bad("authentication failed (wrong key?)"))?;
    Msg::decode(&pt).ok_or_else(|| bad("malformed message"))
}

/// DSCP 46 "Expedited Forwarding" (TOS 0xB8). Routers and Wi-Fi (WMM voice
/// queue) put these packets ahead of bulk traffic such as torrents. On Windows
/// the OS only honours this with a QoS policy, which install.ps1 creates.
const TOS_EF: u32 = 0xB8;

fn tos() -> u32 {
    if crate::flags::on(&crate::flags::QOS) {
        TOS_EF
    } else {
        0
    }
}

pub fn qos_tcp(s: &TcpStream) {
    let _ = socket2::SockRef::from(s).set_tos(tos());
}

pub fn qos_udp(s: &std::net::UdpSocket) {
    let _ = socket2::SockRef::from(s).set_tos(tos());
}

pub fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

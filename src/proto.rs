//! Wire protocol. Hand-rolled little-endian encoding: a tag byte followed by
//! fixed-width fields. Every message is tiny (a mouse move is 9 bytes before
//! encryption), so encode/decode costs nothing next to the network hop.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn right(&self) -> i32 {
        self.x + self.w - 1
    }
    pub fn bottom(&self) -> i32 {
        self.y + self.h - 1
    }
    pub fn center(&self) -> (i32, i32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }
}

pub const BTN_LEFT: u8 = 1;
pub const BTN_RIGHT: u8 = 2;
pub const BTN_MIDDLE: u8 = 3;
pub const BTN_X1: u8 = 4;
pub const BTN_X2: u8 = 5;

#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    /// client -> server, first message of a session.
    Hello { name: String, screen: Rect, udp_port: u16 },
    /// server -> client, accepts the Hello.
    Welcome,
    /// Cursor enters this client at (x, y).
    Enter { x: i32, y: i32 },
    /// Cursor left this client; release everything held.
    Leave,
    /// Absolute cursor position in the client's own coordinates.
    Move { x: i32, y: i32 },
    Button { button: u8, down: bool },
    /// Wheel deltas in Windows units (120 = one notch).
    Wheel { dx: i32, dy: i32 },
    /// USB HID usage code (keyboard page).
    Key { hid: u16, down: bool },
    Clipboard(String),
    UdpProbe(u64),
    UdpOk(u64),
    Ping,
    Pong,
    /// Sequenced input event (Enter/Leave/Button/Wheel/Key). Sent twice over
    /// UDP *and* once over TCP; the client applies each seq exactly once, in
    /// order, from whichever copy arrives first. A lost packet under network
    /// load therefore never waits on a TCP retransmit timeout.
    Seq { seq: u32, inner: Box<Msg> },
}

const T_SEQ: u8 = 14;

const T_HELLO: u8 = 1;
const T_WELCOME: u8 = 2;
const T_ENTER: u8 = 3;
const T_LEAVE: u8 = 4;
const T_MOVE: u8 = 5;
const T_BUTTON: u8 = 6;
const T_WHEEL: u8 = 7;
const T_KEY: u8 = 8;
const T_CLIP: u8 = 9;
const T_PROBE: u8 = 10;
const T_UDPOK: u8 = 11;
const T_PING: u8 = 12;
const T_PONG: u8 = 13;

pub const MAX_CLIPBOARD: usize = 4 * 1024 * 1024;

impl Msg {
    pub fn encode(&self, b: &mut Vec<u8>) {
        match self {
            Msg::Hello { name, screen, udp_port } => {
                b.push(T_HELLO);
                put_rect(b, screen);
                b.extend_from_slice(&udp_port.to_le_bytes());
                put_str(b, name);
            }
            Msg::Welcome => b.push(T_WELCOME),
            Msg::Enter { x, y } => {
                b.push(T_ENTER);
                b.extend_from_slice(&x.to_le_bytes());
                b.extend_from_slice(&y.to_le_bytes());
            }
            Msg::Leave => b.push(T_LEAVE),
            Msg::Move { x, y } => {
                b.push(T_MOVE);
                b.extend_from_slice(&x.to_le_bytes());
                b.extend_from_slice(&y.to_le_bytes());
            }
            Msg::Button { button, down } => {
                b.push(T_BUTTON);
                b.push(*button);
                b.push(*down as u8);
            }
            Msg::Wheel { dx, dy } => {
                b.push(T_WHEEL);
                b.extend_from_slice(&dx.to_le_bytes());
                b.extend_from_slice(&dy.to_le_bytes());
            }
            Msg::Key { hid, down } => {
                b.push(T_KEY);
                b.extend_from_slice(&hid.to_le_bytes());
                b.push(*down as u8);
            }
            Msg::Clipboard(s) => {
                b.push(T_CLIP);
                put_str(b, s);
            }
            Msg::UdpProbe(n) => {
                b.push(T_PROBE);
                b.extend_from_slice(&n.to_le_bytes());
            }
            Msg::UdpOk(n) => {
                b.push(T_UDPOK);
                b.extend_from_slice(&n.to_le_bytes());
            }
            Msg::Ping => b.push(T_PING),
            Msg::Pong => b.push(T_PONG),
            Msg::Seq { seq, inner } => {
                b.push(T_SEQ);
                b.extend_from_slice(&seq.to_le_bytes());
                inner.encode(b);
            }
        }
    }

    pub fn to_vec(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(16);
        self.encode(&mut v);
        v
    }

    pub fn decode(buf: &[u8]) -> Option<Msg> {
        let mut r = Reader(buf);
        let m = match r.u8()? {
            T_HELLO => {
                let screen = r.rect()?;
                let udp_port = r.u16()?;
                let name = r.string()?;
                Msg::Hello { name, screen, udp_port }
            }
            T_WELCOME => Msg::Welcome,
            T_ENTER => Msg::Enter { x: r.i32()?, y: r.i32()? },
            T_LEAVE => Msg::Leave,
            T_MOVE => Msg::Move { x: r.i32()?, y: r.i32()? },
            T_BUTTON => Msg::Button { button: r.u8()?, down: r.u8()? != 0 },
            T_WHEEL => Msg::Wheel { dx: r.i32()?, dy: r.i32()? },
            T_KEY => Msg::Key { hid: r.u16()?, down: r.u8()? != 0 },
            T_CLIP => Msg::Clipboard(r.string()?),
            T_PROBE => Msg::UdpProbe(r.u64()?),
            T_UDPOK => Msg::UdpOk(r.u64()?),
            T_PING => Msg::Ping,
            T_PONG => Msg::Pong,
            T_SEQ => {
                let seq = r.u32()?;
                let inner = Msg::decode(r.0)?;
                if matches!(inner, Msg::Seq { .. }) {
                    return None;
                }
                return Some(Msg::Seq { seq, inner: Box::new(inner) });
            }
            _ => return None,
        };
        if r.0.is_empty() {
            Some(m)
        } else {
            None
        }
    }
}

fn put_rect(b: &mut Vec<u8>, r: &Rect) {
    for v in [r.x, r.y, r.w, r.h] {
        b.extend_from_slice(&v.to_le_bytes());
    }
}

fn put_str(b: &mut Vec<u8>, s: &str) {
    b.extend_from_slice(&(s.len() as u32).to_le_bytes());
    b.extend_from_slice(s.as_bytes());
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn rect(&mut self) -> Option<Rect> {
        Some(Rect { x: self.i32()?, y: self.i32()?, w: self.i32()?, h: self.i32()? })
    }
    fn string(&mut self) -> Option<String> {
        let n = self.u32()? as usize;
        if n > MAX_CLIPBOARD {
            return None;
        }
        String::from_utf8(self.take(n)?.to_vec()).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_all() {
        let msgs = vec![
            Msg::Hello { name: "laptop".into(), screen: Rect { x: -1920, y: 0, w: 3840, h: 1080 }, udp_port: 5000 },
            Msg::Welcome,
            Msg::Enter { x: 1, y: -2 },
            Msg::Leave,
            Msg::Move { x: 100, y: 200 },
            Msg::Button { button: BTN_X2, down: true },
            Msg::Wheel { dx: -120, dy: 240 },
            Msg::Key { hid: 0xE3, down: false },
            Msg::Clipboard("héllo ✓".into()),
            Msg::UdpProbe(7),
            Msg::UdpOk(7),
            Msg::Ping,
            Msg::Pong,
            Msg::Seq { seq: 42, inner: Box::new(Msg::Key { hid: 4, down: true }) },
        ];
        for m in msgs {
            assert_eq!(Msg::decode(&m.to_vec()), Some(m));
        }
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(Msg::decode(&[]), None);
        assert_eq!(Msg::decode(&[T_MOVE, 1, 2]), None);
        assert_eq!(Msg::decode(&[T_PING, 0]), None);
        assert_eq!(Msg::decode(&[200]), None);
    }
}

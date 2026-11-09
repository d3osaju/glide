//! Audio sharing: stream any capture source (system output via WASAPI
//! loopback, a microphone, or a named device) to another machine's output
//! device as encrypted, uncompressed 16-bit PCM over UDP.
//!
//! - 5 ms packets (240 frames @ 48 kHz stereo = 960 bytes, under one MTU).
//! - Receiver keeps a small jitter buffer (default 30 ms) and drops the
//!   oldest audio when it drifts over target, so latency can't creep up.
//! - Uncompressed PCM costs ~1.5 Mbit/s: nothing on a LAN, and no codec delay.
//!
//! To make shared audio show up as a *microphone* on the receiver, play it
//! into a virtual cable (VB-Audio "CABLE Input" on Windows, BlackHole on macOS)
//! and select that cable's output as the mic in your apps.

use crate::crypto::{self, Key, Opener, Sealer};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat};
use std::collections::VecDeque;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const MAGIC: &[u8; 4] = b"GLA1";
const LABEL: &str = "glide v1 audio";
const HEADER: usize = 4 + 16; // magic + salt

#[derive(serde::Serialize, Default)]
pub struct Devices {
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub default_input: Option<String>,
    pub default_output: Option<String>,
}

pub fn devices() -> Devices {
    let host = cpal::default_host();
    fn names<I: Iterator<Item = cpal::Device>>(it: Result<I, cpal::DevicesError>) -> Vec<String> {
        it.map(|d| d.filter_map(|d| d.name().ok()).collect()).unwrap_or_default()
    }
    Devices {
        inputs: names(host.input_devices()),
        outputs: names(host.output_devices()),
        default_input: host.default_input_device().and_then(|d| d.name().ok()),
        default_output: host.default_output_device().and_then(|d| d.name().ok()),
    }
}

pub fn list_devices() -> Result<(), String> {
    let d = devices();
    println!("Input devices (use with audio-send --source \"<name>\"):");
    for n in &d.inputs {
        println!("  {}{}", n, if Some(n) == d.default_input.as_ref() { "  [default mic]" } else { "" });
    }
    println!("Output devices (audio-recv --output \"<name>\"; audio-send --source \"<name>\" captures it via loopback on Windows):");
    for n in &d.outputs {
        println!("  {}{}", n, if Some(n) == d.default_output.as_ref() { "  [default output]" } else { "" });
    }
    Ok(())
}

/// A running audio stream; `stop()` ends it. The cpal stream lives on its own
/// thread because it isn't `Send` on every platform.
pub struct Running {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    pub label: String,
}

impl Running {
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn spawn<F>(name: &str, f: F) -> Result<Running, String>
where
    F: FnOnce(&AtomicBool, &dyn Fn(Result<String, String>)) + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel();
    let st = stop.clone();
    let thread = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let report = move |r| {
                let _ = tx.send(r);
            };
            f(&st, &report)
        })
        .map_err(|e| e.to_string())?;
    match rx.recv_timeout(std::time::Duration::from_secs(10)) {
        Ok(Ok(label)) => Ok(Running { stop, thread: Some(thread), label }),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => Err("audio device did not start".into()),
    }
}

fn find(devs: impl Iterator<Item = cpal::Device>, needle: &str) -> Option<cpal::Device> {
    let needle = needle.to_lowercase();
    devs.into_iter().find(|d| d.name().map(|n| n.to_lowercase().contains(&needle)).unwrap_or(false))
}

/// Resolve `source` to (device, config). "loopback" = what this PC is playing,
/// "mic" = default input, anything else = device name substring.
fn capture_device(source: &str) -> Result<(cpal::Device, cpal::SupportedStreamConfig, String), String> {
    let host = cpal::default_host();
    let (dev, loopback) = match source {
        "loopback" | "system" => (host.default_output_device().ok_or("no default output device")?, true),
        "mic" | "default" => (host.default_input_device().ok_or("no default input device")?, false),
        name => {
            if let Some(d) = find(host.input_devices().map_err(|e| e.to_string())?, name) {
                (d, false)
            } else if let Some(d) = find(host.output_devices().map_err(|e| e.to_string())?, name) {
                (d, true)
            } else {
                return Err(format!("no audio device matching '{name}' (see `glide audio-devices`)"));
            }
        }
    };
    if loopback && !cfg!(windows) {
        return Err("system-output loopback needs WASAPI (Windows). On macOS route output through \
                    BlackHole and use --source BlackHole"
            .into());
    }
    let cfg = if loopback { dev.default_output_config() } else { dev.default_input_config() }
        .map_err(|e| format!("device config: {e}"))?;
    let label = format!("{}{}", dev.name().unwrap_or_default(), if loopback { " (loopback)" } else { "" });
    Ok((dev, cfg, label))
}

struct Packetizer {
    sock: UdpSocket,
    sealer: Sealer,
    salt: [u8; 16],
    rate: u32,
    in_ch: usize,
    out_ch: usize,
    frames_per_packet: usize,
    acc: Vec<i16>,
    plain: Vec<u8>,
}

impl Packetizer {
    fn push<T: Sample>(&mut self, data: &[T])
    where
        f32: FromSample<T>,
    {
        for frame in data.chunks_exact(self.in_ch) {
            for c in 0..self.out_ch {
                let v: f32 = f32::from_sample(frame[c.min(self.in_ch - 1)]);
                self.acc.push((v.clamp(-1.0, 1.0) * i16::MAX as f32) as i16);
            }
            if self.acc.len() >= self.frames_per_packet * self.out_ch {
                self.flush();
            }
        }
    }

    fn flush(&mut self) {
        self.plain.clear();
        self.plain.extend_from_slice(&self.rate.to_le_bytes());
        self.plain.push(self.out_ch as u8);
        for s in self.acc.drain(..) {
            self.plain.extend_from_slice(&s.to_le_bytes());
        }
        let sealed = self.sealer.seal(&self.plain);
        let mut pkt = Vec::with_capacity(HEADER + sealed.len());
        pkt.extend_from_slice(MAGIC);
        pkt.extend_from_slice(&self.salt);
        pkt.extend_from_slice(&sealed);
        let _ = self.sock.send(&pkt);
    }
}

pub fn send(to: &str, source: &str, psk: Key) -> Result<(), String> {
    let r = start_send(to, source, psk)?;
    log::info!("{}; Ctrl+C to stop", r.label);
    loop {
        std::thread::park();
    }
}

pub fn start_send(to: &str, source: &str, psk: Key) -> Result<Running, String> {
    let (to, source) = (to.to_string(), source.to_string());
    spawn("audio-send", move |stop, report| match send_inner(&to, &source, psk, stop, report) {
        Ok(()) => {}
        Err(e) => report(Err(e)),
    })
}

fn send_inner(to: &str, source: &str, psk: Key, stop: &AtomicBool, report: &dyn Fn(Result<String, String>)) -> Result<(), String> {
    let (dev, cfg, label) = capture_device(source)?;
    let sock = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    crate::net::qos_udp(&sock);
    sock.connect(to).map_err(|e| format!("{to}: {e}"))?;
    let salt: [u8; 16] = crypto::random();
    let rate = cfg.sample_rate().0;
    let in_ch = cfg.channels() as usize;
    let out_ch = in_ch.min(2);
    let pk = Arc::new(Mutex::new(Packetizer {
        sock,
        sealer: Sealer::new(&crypto::derive(&psk, LABEL, &salt)),
        salt,
        rate,
        in_ch,
        out_ch,
        frames_per_packet: (rate / 200) as usize,
        acc: Vec::new(),
        plain: Vec::new(),
    }));
    let stream_cfg: cpal::StreamConfig = cfg.config();
    let err = |e| log::warn!("audio capture error: {e}");
    macro_rules! build {
        ($t:ty) => {{
            let pk = pk.clone();
            dev.build_input_stream(&stream_cfg, move |d: &[$t], _| pk.lock().unwrap().push(d), err, None)
        }};
    }
    let stream = match cfg.sample_format() {
        SampleFormat::F32 => build!(f32),
        SampleFormat::I16 => build!(i16),
        SampleFormat::U16 => build!(u16),
        SampleFormat::I32 => build!(i32),
        f => return Err(format!("unsupported sample format {f:?}")),
    }
    .map_err(|e| {
        let hint = if e.to_string().contains("0x80070005") {
            "\n  Windows blocked microphone access: Settings > Privacy & security > Microphone > \
             turn on \"Let desktop apps access your microphone\""
        } else {
            ""
        };
        format!("opening {label}: {e}{hint}")
    })?;
    stream.play().map_err(|e| e.to_string())?;
    report(Ok(format!("streaming {label} ({rate} Hz, {out_ch} ch) -> {to}")));
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    drop(stream);
    Ok(())
}

/// Stateful linear resampler + channel mapper into the playback queue.
struct Resampler {
    in_rate: u32,
    in_ch: usize,
    pos: f64,
    prev: Vec<f32>,
}

impl Resampler {
    fn new(in_rate: u32, in_ch: usize) -> Self {
        Resampler { in_rate, in_ch, pos: 0.0, prev: vec![0.0; in_ch] }
    }

    fn map_channels(frame: &[f32], out_ch: usize, q: &mut VecDeque<f32>) {
        for c in 0..out_ch {
            let v = match (frame.len(), out_ch) {
                (2, 1) => (frame[0] + frame[1]) * 0.5,
                (1, _) if c < 2 => frame[0],
                (n, _) if c < n => frame[c],
                _ => 0.0,
            };
            q.push_back(v);
        }
    }

    fn process(&mut self, input: &[f32], out_rate: u32, out_ch: usize, q: &mut VecDeque<f32>) {
        let ch = self.in_ch;
        let n = input.len() / ch;
        if n == 0 {
            return;
        }
        if self.in_rate == out_rate {
            for f in input.chunks_exact(ch) {
                Self::map_channels(f, out_ch, q);
            }
            return;
        }
        // Virtual buffer: [prev, input[0], ..., input[n-1]]
        let at = |i: usize, c: usize| if i == 0 { self.prev[c] } else { input[(i - 1) * ch + c] };
        let step = self.in_rate as f64 / out_rate as f64;
        let mut frame = vec![0.0f32; ch];
        while self.pos + 1.0 <= n as f64 {
            let i = self.pos.floor() as usize;
            let t = (self.pos - i as f64) as f32;
            for c in 0..ch {
                frame[c] = at(i, c) * (1.0 - t) + at(i + 1, c) * t;
            }
            Self::map_channels(&frame, out_ch, q);
            self.pos += step;
        }
        self.pos -= n as f64;
        self.prev.copy_from_slice(&input[(n - 1) * ch..n * ch]);
    }
}

struct Jitter {
    q: VecDeque<f32>,
    primed: bool,
}

pub fn recv(listen: &str, output: Option<&str>, latency_ms: u32, psk: Key) -> Result<(), String> {
    let r = start_recv(listen, output, latency_ms, psk)?;
    log::info!("{}", r.label);
    loop {
        std::thread::park();
    }
}

pub fn start_recv(listen: &str, output: Option<&str>, latency_ms: u32, psk: Key) -> Result<Running, String> {
    let (listen, output) = (listen.to_string(), output.map(str::to_string));
    spawn("audio-recv", move |stop, report| {
        if let Err(e) = recv_inner(&listen, output.as_deref(), latency_ms, psk, stop, report) {
            report(Err(e));
        }
    })
}

fn recv_inner(
    listen: &str,
    output: Option<&str>,
    latency_ms: u32,
    psk: Key,
    stop: &AtomicBool,
    report: &dyn Fn(Result<String, String>),
) -> Result<(), String> {
    let host = cpal::default_host();
    let dev = match output {
        None => host.default_output_device().ok_or("no default output device")?,
        Some(n) => find(host.output_devices().map_err(|e| e.to_string())?, n)
            .ok_or_else(|| format!("no output device matching '{n}' (see `glide audio-devices`)"))?,
    };
    let cfg = dev.default_output_config().map_err(|e| e.to_string())?;
    let out_rate = cfg.sample_rate().0;
    let out_ch = cfg.channels() as usize;
    let target = (latency_ms as usize * out_rate as usize / 1000) * out_ch;
    let max = target * 2 + out_rate as usize / 50 * out_ch; // target*2 + 20 ms
    let jit = Arc::new(Mutex::new(Jitter { q: VecDeque::with_capacity(max * 2), primed: false }));

    let stream_cfg: cpal::StreamConfig = cfg.config();
    let err = |e| log::warn!("audio playback error: {e}");
    macro_rules! build {
        ($t:ty) => {{
            let jit = jit.clone();
            dev.build_output_stream(&stream_cfg, move |d: &mut [$t], _| fill(d, &jit, target), err, None)
        }};
    }
    let stream = match cfg.sample_format() {
        SampleFormat::F32 => build!(f32),
        SampleFormat::I16 => build!(i16),
        SampleFormat::U16 => build!(u16),
        SampleFormat::I32 => build!(i32),
        f => return Err(format!("unsupported sample format {f:?}")),
    }
    .map_err(|e| format!("opening output: {e}"))?;
    stream.play().map_err(|e| e.to_string())?;

    let sock = UdpSocket::bind(listen).map_err(|e| format!("bind {listen}: {e}"))?;
    sock.set_read_timeout(Some(std::time::Duration::from_millis(200))).map_err(|e| e.to_string())?;
    crate::net::qos_udp(&sock);
    report(Ok(format!(
        "playing to '{}' ({out_rate} Hz, {out_ch} ch), listening on {listen}, buffer {latency_ms} ms",
        dev.name().unwrap_or_default()
    )));
    crate::platform::boost_thread();

    let mut buf = [0u8; 2048];
    let mut session: Option<([u8; 16], Opener, Resampler)> = None;
    let mut samples = Vec::<f32>::new();
    while !stop.load(Ordering::Relaxed) {
        let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
        let pkt = &buf[..n];
        if n < HEADER || &pkt[..4] != MAGIC {
            continue;
        }
        let salt: [u8; 16] = pkt[4..20].try_into().unwrap();
        if session.as_ref().map(|s| s.0) != Some(salt) {
            // New sender session; only switch once a packet authenticates.
            let mut op = Opener::new(&crypto::derive(&psk, LABEL, &salt), false);
            let Some(pt) = op.open(&pkt[HEADER..]) else { continue };
            let Some((rate, ch, _)) = parse(&pt) else { continue };
            log::info!("audio stream from {from}: {rate} Hz, {ch} ch");
            session = Some((salt, op, Resampler::new(rate, ch)));
            jit.lock().unwrap().q.clear();
            continue;
        }
        let (_, op, rs) = session.as_mut().unwrap();
        let Some(pt) = op.open(&pkt[HEADER..]) else { continue };
        let Some((rate, ch, pcm)) = parse(&pt) else { continue };
        if rate != rs.in_rate || ch != rs.in_ch {
            *rs = Resampler::new(rate, ch);
        }
        samples.clear();
        samples.extend(pcm.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / i16::MAX as f32));
        let mut j = jit.lock().unwrap();
        rs.process(&samples, out_rate, out_ch, &mut j.q);
        if j.q.len() > max {
            // Clock drift or a burst: jump back to the target latency.
            let drop = j.q.len() - target;
            let drop = drop - drop % out_ch;
            j.q.drain(..drop);
        }
    }
    std::mem::drop(stream);
    Ok(())
}

fn parse(pt: &[u8]) -> Option<(u32, usize, &[u8])> {
    if pt.len() < 5 {
        return None;
    }
    let rate = u32::from_le_bytes(pt[..4].try_into().ok()?);
    let ch = pt[4] as usize;
    if !(8000..=192_000).contains(&rate) || !(1..=2).contains(&ch) {
        return None;
    }
    Some((rate, ch, &pt[5..]))
}

fn fill<T: Sample + FromSample<f32>>(out: &mut [T], jit: &Mutex<Jitter>, target: usize) {
    let mut j = jit.lock().unwrap();
    if !j.primed && j.q.len() >= target {
        j.primed = true;
    }
    for s in out.iter_mut() {
        let v = if j.primed { j.q.pop_front() } else { None };
        if v.is_none() {
            j.primed = false; // underrun: re-buffer to target before resuming
        }
        *s = T::from_sample(v.unwrap_or(0.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_44k_to_48k_length() {
        let mut r = Resampler::new(44_100, 2);
        let mut q = VecDeque::new();
        let input = vec![0.5f32; 441 * 2 * 100]; // 1 s
        for chunk in input.chunks(441 * 2) {
            r.process(chunk, 48_000, 2, &mut q);
        }
        let frames = q.len() / 2;
        assert!((47_990..=48_010).contains(&frames), "{frames}");
        assert!(q.iter().skip(10).all(|&v| (v - 0.5).abs() < 1e-4));
    }

    #[test]
    fn mono_to_stereo() {
        let mut r = Resampler::new(48_000, 1);
        let mut q = VecDeque::new();
        r.process(&[0.25, -0.25], 48_000, 2, &mut q);
        assert_eq!(q.into_iter().collect::<Vec<_>>(), vec![0.25, 0.25, -0.25, -0.25]);
    }
}


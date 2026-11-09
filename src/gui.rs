//! Desktop app (Tauri): pick "Host" or "Connect", tray icon, every option
//! switchable at runtime. The UI (ui/index.html) polls `get_state`.

use crate::config::{Dir, Layout};
use crate::{audio, client, crypto, flags, pair, server, update};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, State, WindowEvent, Wry};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};

pub const AUDIO_PORT: u16 = 24810;

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
struct Settings {
    name: String,
    port: u16,
    key: String,
    placements: Vec<Placement>,
    /// "host" | "client" | none: what to resume on launch.
    mode: Option<String>,
    last_code: Option<String>,
    // sharing
    clipboard: bool,
    swap_ctrl_cmd: bool,
    block_drag: bool,
    hotkey: bool,
    discoverable: bool,
    // network
    qos: bool,
    keepalive: bool,
    // app
    resume: bool,
    start_minimized: bool,
    close_to_tray: bool,
    minimize_to_tray: bool,
    // audio
    audio_recv: bool,
    audio_output: Option<String>,
    audio_latency: u32,
    audio_send: bool,
    audio_source: String,
    audio_target: Option<String>,
    audio_mute_local: bool,
    auto_update: bool,
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
struct Placement {
    name: String,
    anchor: String,
    side: Dir,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            name: hostname(),
            port: 24800,
            key: crypto::gen_key(),
            placements: Vec::new(),
            mode: None,
            last_code: None,
            clipboard: true,
            swap_ctrl_cmd: cfg!(target_os = "macos"),
            block_drag: true,
            hotkey: true,
            discoverable: true,
            qos: true,
            keepalive: true,
            resume: true,
            start_minimized: false,
            close_to_tray: true,
            minimize_to_tray: true,
            audio_recv: false,
            audio_output: None,
            audio_latency: 30,
            audio_send: false,
            audio_source: if cfg!(windows) { "loopback".into() } else { "mic".into() },
            audio_target: None,
            audio_mute_local: true,
            auto_update: true,
        }
    }
}

fn hostname() -> String {
    let h = gethostname::gethostname().to_string_lossy().to_string();
    let h = h.split('.').next().unwrap_or("").trim().to_string();
    if h.is_empty() {
        "my-pc".into()
    } else {
        h
    }
}

struct Hosting {
    handle: server::ServerHandle,
    beacon_stop: Arc<AtomicBool>,
}

struct Connected {
    ctl: Arc<client::Ctl>,
    server: String,
}

#[derive(Default, Clone, Serialize)]
struct UpdState {
    current: &'static str,
    checking: bool,
    checked: bool,
    available: Option<update::Release>,
    error: Option<String>,
    installing: bool,
    downloaded: u64,
    #[serde(skip)]
    last_check: Option<Instant>,
}

#[derive(Default)]
struct AudioSlot {
    running: Option<(audio::Running, String)>,
    error: Option<String>,
    failed: Option<(String, Instant)>,
}

pub struct App {
    settings: Mutex<Settings>,
    path: PathBuf,
    layout: Arc<RwLock<Layout>>,
    host: Mutex<Option<Hosting>>,
    client: Mutex<Option<Connected>>,
    disco: Arc<pair::Discovery>,
    tray_status: Mutex<Option<MenuItem<Wry>>>,
    audio_rx: Mutex<AudioSlot>,
    audio_tx: Mutex<AudioSlot>,
    upd: Mutex<UpdState>,
    started: Instant,
    audio_sync: Mutex<()>,
}

impl App {
    fn load(path: PathBuf) -> Self {
        let mut settings: Settings = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        if crypto::parse_key(&settings.key).is_err() {
            settings.key = crypto::gen_key();
        }
        let app = App {
            layout: Arc::new(RwLock::new(Layout::new(&settings.name))),
            settings: Mutex::new(settings),
            path,
            host: Mutex::new(None),
            client: Mutex::new(None),
            disco: Arc::new(pair::Discovery::default()),
            tray_status: Mutex::new(None),
            audio_rx: Mutex::new(AudioSlot::default()),
            audio_tx: Mutex::new(AudioSlot::default()),
            upd: Mutex::new(UpdState { current: update::current_version(), ..Default::default() }),
            started: Instant::now(),
            audio_sync: Mutex::new(()),
        };
        app.apply_flags();
        app.save();
        app.rebuild_layout();
        app
    }

    fn apply_flags(&self) {
        let s = self.settings.lock().unwrap();
        flags::set(&flags::CLIPBOARD, s.clipboard);
        flags::set(&flags::BLOCK_DRAG, s.block_drag);
        flags::set(&flags::HOTKEY, s.hotkey);
        flags::set(&flags::DISCOVERABLE, s.discoverable);
        flags::set(&flags::QOS, s.qos);
        flags::set(&flags::KEEPALIVE, s.keepalive);
    }

    fn save(&self) {
        let s = self.settings.lock().unwrap().clone();
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(t) = serde_json::to_string_pretty(&s) {
            let _ = std::fs::write(&self.path, t);
        }
    }

    fn rebuild_layout(&self) {
        let s = self.settings.lock().unwrap();
        let mut l = Layout::new(&s.name);
        // Place in dependency order so chains (A right of B right of host) resolve.
        let mut placed = vec![s.name.clone()];
        let mut left: Vec<&Placement> = s.placements.iter().collect();
        while !left.is_empty() {
            let before = left.len();
            left.retain(|p| {
                if placed.contains(&p.anchor) {
                    l.place(&p.name, &p.anchor, p.side);
                    placed.push(p.name.clone());
                    false
                } else {
                    true
                }
            });
            if left.len() == before {
                for p in left.drain(..) {
                    l.place(&p.name, &s.name, p.side);
                }
            }
        }
        *self.layout.write().unwrap() = l;
    }

    /// New devices get the first free side of the host, then chain to the right.
    fn auto_place(&self, devices: &[server::DeviceInfo]) {
        let mut changed = false;
        {
            let mut s = self.settings.lock().unwrap();
            for d in devices {
                if d.name == s.name || s.placements.iter().any(|p| p.name == d.name) {
                    continue;
                }
                let me = s.name.clone();
                let taken = |s: &Settings, a: &str, dir: Dir| s.placements.iter().any(|p| p.anchor == a && p.side == dir);
                let side = [Dir::Right, Dir::Left, Dir::Up, Dir::Down].into_iter().find(|dir| !taken(&s, &me, *dir));
                let (anchor, side) = match side {
                    Some(dir) => (me, dir),
                    None => {
                        let mut a = me;
                        while let Some(p) = s.placements.iter().find(|p| p.anchor == a && p.side == Dir::Right) {
                            a = p.name.clone();
                        }
                        (a, Dir::Right)
                    }
                };
                s.placements.push(Placement { name: d.name.clone(), anchor, side });
                changed = true;
            }
        }
        if changed {
            self.save();
            self.rebuild_layout();
        }
    }

    fn start_host(&self) -> Result<(), String> {
        self.stop_all();
        let (port, key, name) = {
            let s = self.settings.lock().unwrap();
            (s.port, crypto::parse_key(&s.key)?, s.name.clone())
        };
        self.rebuild_layout();
        let handle = server::start(server::ServerOpts {
            listen: format!("0.0.0.0:{port}"),
            psk: key,
            clipboard: true, // gated live by flags::CLIPBOARD
            layout: self.layout.clone(),
        })
        .map_err(|e| {
            if e.contains("10048") || e.to_lowercase().contains("address already in use") {
                format!("Port {port} is already in use. Is another glide already hosting?")
            } else {
                e
            }
        })?;
        let beacon_stop = Arc::new(AtomicBool::new(false));
        pair::spawn_beacon(name, handle.port, pair::fingerprint(&key), beacon_stop.clone());
        *self.host.lock().unwrap() = Some(Hosting { handle, beacon_stop });
        self.settings.lock().unwrap().mode = Some("host".into());
        self.save();
        Ok(())
    }

    fn connect(&self, code: &str) -> Result<(), String> {
        let (addr, key) = pair::decode_code(code)?;
        self.stop_all();
        let (name, swap) = {
            let mut s = self.settings.lock().unwrap();
            s.last_code = Some(code.trim().to_string());
            s.mode = Some("client".into());
            (s.name.clone(), s.swap_ctrl_cmd)
        };
        self.save();
        let ctl = client::Ctl::new();
        let opts = client::Options { server: addr.to_string(), name, key, swap_ctrl_cmd: swap, clipboard: true };
        {
            let ctl = ctl.clone();
            std::thread::Builder::new().name("client".into()).spawn(move || client::run(opts, ctl)).map_err(|e| e.to_string())?;
        }
        *self.client.lock().unwrap() = Some(Connected { ctl, server: addr.to_string() });
        Ok(())
    }

    fn stop_all(&self) {
        if let Some(h) = self.host.lock().unwrap().take() {
            h.beacon_stop.store(true, Ordering::Relaxed);
            h.handle.stop();
        }
        if let Some(c) = self.client.lock().unwrap().take() {
            c.ctl.stop();
        }
        self.stop_audio();
    }

    /// Stopping is quick and must happen before quitting so a muted speaker is restored.
    fn stop_audio(&self) {
        for slot in [&self.audio_rx, &self.audio_tx] {
            let r = {
                let mut s = slot.lock().unwrap();
                s.error = None;
                s.failed = None;
                s.running.take()
            };
            if let Some((r, _)) = r {
                r.stop();
            }
        }
    }

    /// Apply audio changes soon, off the UI thread (opening a device can take seconds).
    fn kick_audio(self: &Arc<Self>) {
        let me = self.clone();
        std::thread::spawn(move || me.sync_audio());
    }

    /// Key and peer for the current session (audio rides on it).
    fn session(&self) -> Option<(crypto::Key, Option<std::net::IpAddr>)> {
        let s = self.settings.lock().unwrap().clone();
        if let Some(h) = self.host.lock().unwrap().as_ref() {
            let key = crypto::parse_key(&s.key).ok()?;
            let devs = h.handle.reg.devices();
            let target = s
                .audio_target
                .as_ref()
                .and_then(|t| devs.iter().find(|d| &d.name == t))
                .or_else(|| devs.first())
                .map(|d| d.ip);
            return Some((key, target));
        }
        if self.client.lock().unwrap().is_some() {
            let (addr, key) = pair::decode_code(s.last_code.as_deref()?).ok()?;
            return Some((key, Some(addr.ip())));
        }
        None
    }

    /// Start/stop/restart audio streams so they match the settings and session.
    fn sync_audio(&self) {
        // One sync at a time; if one is busy opening a device, the next tick catches up.
        let Ok(_one_at_a_time) = self.audio_sync.try_lock() else { return };
        let s = self.settings.lock().unwrap().clone();
        let session = self.session();

        // "System default" means *the current* default: include its name in the
        // signature so plugging in headphones restarts the stream on them.
        let default_out = if (s.audio_recv && s.audio_output.is_none()) || (s.audio_send && s.audio_source == "loopback") {
            audio::default_output_name()
        } else {
            None
        };

        // Receive
        let want_rx = session.filter(|_| s.audio_recv).map(|(key, _)| {
            let out = s.audio_output.clone().or_else(|| default_out.clone());
            (format!("{:?}|{}|{:?}", out, s.audio_latency, key), key)
        });
        let rx_out = s.audio_output.clone().or_else(|| default_out.clone());
        // Echo guard: playing received audio into the very device whose sound we
        // are capturing would loop it straight back to the other side.
        let echo = s.audio_recv && s.audio_send && s.audio_source == "loopback" && rx_out.is_some() && rx_out == default_out;
        let want_rx = if echo {
            let old = self.audio_rx.lock().unwrap().running.take();
            if let Some((r, _)) = old {
                r.stop();
            }
            let mut slot = self.audio_rx.lock().unwrap();
            slot.error = Some(
                "Not playing: this output is also being sent, which would echo back. Pick another Output, such as a virtual cable (CABLE Input)."
                    .into(),
            );
            None
        } else {
            want_rx
        };
        if !echo {
            Self::reconcile(&self.audio_rx, want_rx, |key| {
                audio::start_recv(&format!("0.0.0.0:{AUDIO_PORT}"), rx_out.as_deref(), s.audio_latency, key)
            });
        }

        // Send
        let want_tx = match session.filter(|_| s.audio_send) {
            Some((key, Some(ip))) => Some((
                format!("{}|{:?}|{}|{}|{:?}", s.audio_source, default_out, s.audio_mute_local, ip, key),
                (key, ip),
            )),
            Some((_, None)) => {
                let old = self.audio_tx.lock().unwrap().running.take();
                if let Some((r, _)) = old {
                    r.stop();
                }
                self.audio_tx.lock().unwrap().error = Some("Waiting for a device to connect".into());
                return;
            }
            None => None,
        };
        Self::reconcile(&self.audio_tx, want_tx, |(key, ip)| {
            audio::start_send(&format!("{ip}:{AUDIO_PORT}"), &s.audio_source, s.audio_mute_local, key)
        });
    }

    fn reconcile<K>(slot: &Mutex<AudioSlot>, want: Option<(String, K)>, start: impl FnOnce(K) -> Result<audio::Running, String>) {
        let (current, failed) = {
            let s = slot.lock().unwrap();
            (s.running.as_ref().map(|(_, sig)| sig.clone()), s.failed.clone())
        };
        let Some((sig, arg)) = want else {
            let old = {
                let mut s = slot.lock().unwrap();
                s.error = None;
                s.failed = None;
                s.running.take()
            };
            if let Some((r, _)) = old {
                r.stop();
            }
            return;
        };
        if current.as_deref() == Some(sig.as_str()) {
            return;
        }
        if let Some((fsig, at)) = &failed {
            if *fsig == sig && at.elapsed() < Duration::from_secs(5) {
                return; // don't hammer a device that just failed
            }
        }
        let old = slot.lock().unwrap().running.take();
        if let Some((r, _)) = old {
            r.stop();
        }
        // Opening the device can take a while: no lock is held here.
        let result = start(arg);
        let mut s = slot.lock().unwrap();
        match result {
            Ok(r) => {
                log::info!("{}", r.label);
                s.running = Some((r, sig));
                s.error = None;
                s.failed = None;
            }
            Err(e) => {
                log::warn!("audio: {e}");
                s.error = Some(e);
                s.failed = Some((sig, Instant::now()));
            }
        }
    }

    /// True while the cursor is on another device (never restart then).
    fn in_use(&self) -> bool {
        if let Some(h) = self.host.lock().unwrap().as_ref() {
            if h.handle.reg.active().is_some() {
                return true;
            }
        }
        if let Some(c) = self.client.lock().unwrap().as_ref() {
            if c.ctl.active.load(Ordering::Relaxed) {
                return true;
            }
        }
        false
    }

    fn check_update_async(self: &Arc<Self>) {
        {
            let mut u = self.upd.lock().unwrap();
            if u.checking || u.installing {
                return;
            }
            u.checking = true;
            u.error = None;
        }
        let me = self.clone();
        std::thread::spawn(move || {
            let r = update::check();
            let mut u = me.upd.lock().unwrap();
            u.checking = false;
            u.checked = true;
            u.last_check = Some(Instant::now());
            match r {
                Ok(a) => u.available = a,
                Err(e) => u.error = Some(e),
            }
        });
    }

    fn install_update_async(self: &Arc<Self>, app: AppHandle) {
        let rel = {
            let mut u = self.upd.lock().unwrap();
            if u.installing {
                return;
            }
            let Some(r) = u.available.clone() else { return };
            u.installing = true;
            u.error = None;
            u.downloaded = 0;
            r
        };
        let me = self.clone();
        std::thread::spawn(move || {
            let res = update::install(&rel, &|n| me.upd.lock().unwrap().downloaded = n);
            match res {
                Ok(exe) => {
                    log::info!("updated to {}; restarting", rel.version);
                    let hidden = app
                        .get_webview_window("main")
                        .map(|w| !w.is_visible().unwrap_or(true))
                        .unwrap_or(true);
                    me.stop_all(); // session mode is kept, so it resumes after restart
                    match update::relaunch(&exe, hidden) {
                        Ok(()) => app.exit(0),
                        Err(e) => {
                            let mut u = me.upd.lock().unwrap();
                            u.installing = false;
                            u.error = Some(format!("updated, but couldn't restart: {e}. Please reopen glide."));
                        }
                    }
                }
                Err(e) => {
                    log::warn!("update failed: {e}");
                    let mut u = me.upd.lock().unwrap();
                    u.installing = false;
                    u.error = Some(e);
                }
            }
        });
    }

    fn summary(&self) -> String {
        if let Some(h) = self.host.lock().unwrap().as_ref() {
            let n = h.handle.reg.devices().len();
            return match n {
                0 => "Hosting: waiting for devices".into(),
                1 => "Hosting: 1 device connected".into(),
                n => format!("Hosting: {n} devices connected"),
            };
        }
        if let Some(c) = self.client.lock().unwrap().as_ref() {
            return match c.ctl.status() {
                client::Status::Connected => format!("Connected to {}", c.server),
                client::Status::Connecting => "Connecting…".into(),
                client::Status::Error(e) => format!("Retrying: {e}"),
                client::Status::Stopped => "Disconnected".into(),
            };
        }
        "Not sharing".into()
    }
}

// ---------- view model ----------

#[derive(Serialize)]
struct View {
    name: String,
    mode: &'static str,
    host: Option<HostView>,
    client: Option<ClientView>,
    found: Vec<pair::FoundHost>,
    last_code: Option<String>,
    last_fp: Option<String>,
    settings: Settings,
    autostart: bool,
    audio: AudioView,
    update: UpdState,
    version: &'static str,
}

#[derive(Serialize)]
struct AudioView {
    rx_on: bool,
    rx_label: Option<String>,
    rx_error: Option<String>,
    tx_on: bool,
    tx_label: Option<String>,
    tx_error: Option<String>,
    tx_level: u32,
}

#[derive(Serialize)]
struct HostView {
    code: String,
    ips: Vec<pair::LocalIp>,
    port: u16,
    key: String,
    fingerprint: String,
    active: Option<String>,
    screen: (i32, i32),
    devices: Vec<DeviceView>,
}

#[derive(Serialize)]
struct DeviceView {
    name: String,
    online: bool,
    ip: Option<String>,
    w: i32,
    h: i32,
    anchor: String,
    side: Dir,
}

#[derive(Serialize)]
struct ClientView {
    status: &'static str,
    error: Option<String>,
    server: String,
    active: bool,
}

#[tauri::command]
fn get_state(app: AppHandle, st: State<'_, Arc<App>>) -> View {
    let s = st.settings.lock().unwrap().clone();
    let key = crypto::parse_key(&s.key).unwrap_or([0; 32]);
    let host = st.host.lock().unwrap().as_ref().map(|h| {
        let online = h.handle.reg.devices();
        let ips = pair::local_ips();
        let ip = ips.first().and_then(|i| i.ip.parse().ok()).unwrap_or(std::net::Ipv4Addr::LOCALHOST);
        let local = crate::platform::desktop_bounds();
        let devices = s
            .placements
            .iter()
            .map(|p| {
                let on = online.iter().find(|d| d.name == p.name);
                DeviceView {
                    name: p.name.clone(),
                    online: on.is_some(),
                    ip: on.map(|d| d.ip.to_string()),
                    w: on.map(|d| d.screen.w).unwrap_or(0),
                    h: on.map(|d| d.screen.h).unwrap_or(0),
                    anchor: p.anchor.clone(),
                    side: p.side,
                }
            })
            .collect();
        HostView {
            code: pair::encode_code(ip, h.handle.port, &key),
            ips,
            port: h.handle.port,
            key: s.key.clone(),
            fingerprint: pair::fingerprint(&key),
            active: h.handle.reg.active(),
            screen: (local.w, local.h),
            devices,
        }
    });
    let client = st.client.lock().unwrap().as_ref().map(|c| {
        let (status, error) = match c.ctl.status() {
            client::Status::Connecting => ("connecting", None),
            client::Status::Connected => ("connected", None),
            client::Status::Error(e) => ("error", Some(e)),
            client::Status::Stopped => ("stopped", None),
        };
        ClientView { status, error, server: c.server.clone(), active: c.ctl.active.load(Ordering::Relaxed) }
    });
    let mode = if host.is_some() {
        "host"
    } else if client.is_some() {
        "client"
    } else {
        "idle"
    };
    if mode != "host" {
        st.disco.start();
    }
    let audio = {
        let rx = st.audio_rx.lock().unwrap();
        let tx = st.audio_tx.lock().unwrap();
        AudioView {
            rx_on: rx.running.is_some(),
            rx_label: rx.running.as_ref().map(|r| r.0.label.clone()),
            rx_error: rx.error.clone(),
            tx_on: tx.running.is_some(),
            tx_label: tx.running.as_ref().map(|r| r.0.label.clone()),
            tx_error: tx.error.clone(),
            tx_level: tx.running.as_ref().map(|r| r.0.level()).unwrap_or(0),
        }
    };
    let last_fp = s.last_code.as_deref().and_then(|c| pair::decode_code(c).ok()).map(|(_, k)| pair::fingerprint(&k));
    View {
        name: s.name.clone(),
        mode,
        host,
        client,
        found: {
            // Never list this PC itself (its own beacon lingers a few seconds after hosting stops).
            let mine: Vec<String> = pair::local_ips().into_iter().map(|l| l.ip).collect();
            st.disco.list().into_iter().filter(|f| !mine.iter().any(|ip| f.addr.starts_with(&format!("{ip}:")))).collect()
        },
        last_code: s.last_code.clone(),
        last_fp,
        autostart: app.autolaunch().is_enabled().unwrap_or(false),
        settings: s,
        audio,
        update: st.upd.lock().unwrap().clone(),
        version: env!("CARGO_PKG_VERSION"),
    }
}

#[tauri::command]
fn check_update(st: State<'_, Arc<App>>) {
    st.inner().check_update_async();
}

#[tauri::command]
fn install_update(app: AppHandle, st: State<'_, Arc<App>>) {
    st.inner().install_update_async(app);
}

#[tauri::command]
fn start_host(st: State<'_, Arc<App>>) -> Result<(), String> {
    st.start_host()?;
    st.inner().kick_audio();
    Ok(())
}

#[tauri::command]
fn connect(st: State<'_, Arc<App>>, code: String) -> Result<(), String> {
    st.connect(&code)?;
    st.inner().kick_audio();
    Ok(())
}

#[tauri::command]
fn stop(st: State<'_, Arc<App>>) {
    st.stop_all();
    st.settings.lock().unwrap().mode = None;
    st.save();
}

#[tauri::command]
fn set_name(st: State<'_, Arc<App>>, name: String) -> Result<(), String> {
    let name: String = name.trim().chars().filter(|c| c.is_alphanumeric() || "-_ .".contains(*c)).take(40).collect();
    if name.is_empty() {
        return Err("Name can't be empty".into());
    }
    if st.host.lock().unwrap().is_some() || st.client.lock().unwrap().is_some() {
        return Err("Stop sharing before renaming this device".into());
    }
    {
        let mut s = st.settings.lock().unwrap();
        let old = std::mem::replace(&mut s.name, name.clone());
        for p in s.placements.iter_mut() {
            if p.anchor == old {
                p.anchor = name.clone();
            }
        }
    }
    st.save();
    st.rebuild_layout();
    Ok(())
}

#[tauri::command]
fn set_placement(st: State<'_, Arc<App>>, name: String, anchor: String, side: Dir) -> Result<(), String> {
    if name == anchor {
        return Err("A screen can't sit next to itself".into());
    }
    {
        let mut s = st.settings.lock().unwrap();
        let old = s.placements.iter().find(|p| p.name == name).cloned();
        // If the new anchor hangs off `name`, re-hang it where `name` was (avoid a cycle).
        let mut a = anchor.clone();
        for _ in 0..64 {
            let Some(p) = s.placements.iter().find(|p| p.name == a).cloned() else { break };
            if p.anchor == name {
                if let (Some(o), Some(c)) = (&old, s.placements.iter_mut().find(|q| q.name == a)) {
                    c.anchor = o.anchor.clone();
                    c.side = o.side;
                }
                break;
            }
            a = p.anchor;
        }
        // Whoever already occupies that spot swaps into ours.
        if let Some(o) = &old {
            if let Some(occ) = s.placements.iter_mut().find(|p| p.anchor == anchor && p.side == side && p.name != name) {
                occ.anchor = o.anchor.clone();
                occ.side = o.side;
            }
        }
        match s.placements.iter_mut().find(|p| p.name == name) {
            Some(p) => {
                p.anchor = anchor;
                p.side = side;
            }
            None => s.placements.push(Placement { name, anchor, side }),
        }
    }
    st.save();
    st.rebuild_layout();
    Ok(())
}

#[tauri::command]
fn forget_device(st: State<'_, Arc<App>>, name: String) {
    {
        let mut s = st.settings.lock().unwrap();
        let gone = s.placements.iter().find(|p| p.name == name).cloned();
        s.placements.retain(|p| p.name != name);
        if let Some(g) = gone {
            for p in s.placements.iter_mut().filter(|p| p.anchor == name) {
                p.anchor = g.anchor.clone();
            }
        }
    }
    st.save();
    st.rebuild_layout();
}

#[tauri::command]
fn new_key(st: State<'_, Arc<App>>) -> Result<(), String> {
    let hosting = st.host.lock().unwrap().is_some();
    st.settings.lock().unwrap().key = crypto::gen_key();
    st.save();
    if hosting {
        st.start_host()?;
    }
    Ok(())
}

/// Every option in the UI goes through here; changes apply immediately.
#[tauri::command]
fn set_option(app: AppHandle, st: State<'_, Arc<App>>, key: String, value: Value) -> Result<(), String> {
    let as_bool = || value.as_bool().ok_or_else(|| format!("{key} expects true/false"));
    let as_str = || value.as_str().map(|s| s.to_string()).filter(|s| !s.is_empty());
    if key == "autostart" {
        let al = app.autolaunch();
        return if as_bool()? { al.enable() } else { al.disable() }.map_err(|e| e.to_string());
    }
    let mut restart_client = false;
    {
        let mut s = st.settings.lock().unwrap();
        match key.as_str() {
            "clipboard" => s.clipboard = as_bool()?,
            "swap_ctrl_cmd" => {
                s.swap_ctrl_cmd = as_bool()?;
                restart_client = true;
            }
            "block_drag" => s.block_drag = as_bool()?,
            "hotkey" => s.hotkey = as_bool()?,
            "discoverable" => s.discoverable = as_bool()?,
            "qos" => s.qos = as_bool()?,
            "keepalive" => s.keepalive = as_bool()?,
            "resume" => s.resume = as_bool()?,
            "start_minimized" => s.start_minimized = as_bool()?,
            "close_to_tray" => s.close_to_tray = as_bool()?,
            "minimize_to_tray" => s.minimize_to_tray = as_bool()?,
            "audio_recv" => s.audio_recv = as_bool()?,
            "audio_send" => s.audio_send = as_bool()?,
            "audio_mute_local" => s.audio_mute_local = as_bool()?,
            "auto_update" => s.auto_update = as_bool()?,
            "audio_output" => s.audio_output = as_str(),
            "audio_source" => s.audio_source = as_str().unwrap_or_else(|| "loopback".into()),
            "audio_target" => s.audio_target = as_str(),
            "audio_latency" => {
                s.audio_latency = value.as_u64().ok_or("audio_latency expects a number")?.clamp(10, 500) as u32
            }
            k => return Err(format!("unknown option {k}")),
        }
    }
    st.apply_flags();
    st.save();
    if restart_client {
        let code = st.client.lock().unwrap().as_ref().and(st.settings.lock().unwrap().last_code.clone());
        if let Some(code) = code {
            st.connect(&code)?;
        }
    }
    st.inner().kick_audio();
    Ok(())
}

#[tauri::command]
fn audio_devices() -> audio::Devices {
    audio::devices()
}

#[tauri::command]
fn copy_text(text: String) -> Result<(), String> {
    arboard::Clipboard::new().and_then(|mut c| c.set_text(text)).map_err(|e| e.to_string())
}

#[tauri::command]
fn hide_window(app: AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.hide();
    }
}

#[tauri::command]
fn quit(app: AppHandle, st: State<'_, Arc<App>>) {
    st.stop_all();
    app.exit(0);
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

pub fn run(launched_by_autostart: bool) {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_main(app)))
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, Some(vec!["--tray"])))
        .invoke_handler(tauri::generate_handler![
            get_state,
            start_host,
            connect,
            stop,
            set_name,
            set_placement,
            forget_device,
            new_key,
            set_option,
            audio_devices,
            check_update,
            install_update,
            copy_text,
            hide_window,
            quit
        ])
        .setup(move |app| {
            let path = app.path().app_config_dir()?.join("settings.json");
            let state = Arc::new(App::load(path));
            app.manage(state.clone());

            // Tray
            let status = MenuItem::with_id(app, "status", "Not sharing", false, None::<&str>)?;
            let open = MenuItem::with_id(app, "open", "Open glide", true, None::<&str>)?;
            let stop = MenuItem::with_id(app, "stop", "Stop sharing", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit glide", true, None::<&str>)?;
            let menu = Menu::with_items(
                app,
                &[&status, &PredefinedMenuItem::separator(app)?, &open, &stop, &PredefinedMenuItem::separator(app)?, &quit],
            )?;
            *state.tray_status.lock().unwrap() = Some(status);
            TrayIconBuilder::with_id("main")
                .icon(app.default_window_icon().cloned().ok_or("missing app icon")?)
                .tooltip("glide")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, e| match e.id.as_ref() {
                    "open" => show_main(app),
                    "stop" => {
                        let st = app.state::<Arc<App>>();
                        st.stop_all();
                        st.settings.lock().unwrap().mode = None;
                        st.save();
                    }
                    "quit" => {
                        app.state::<Arc<App>>().stop_all();
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, e| {
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                        show_main(tray.app_handle());
                    }
                })
                .build(app)?;

            // Resume what was running last time.
            let (resume, mode, code, start_min) = {
                let s = state.settings.lock().unwrap();
                (s.resume, s.mode.clone(), s.last_code.clone(), s.start_minimized)
            };
            if resume {
                // In the background so the window appears immediately.
                let st = state.clone();
                std::thread::spawn(move || {
                    let r = match (mode.as_deref(), code) {
                        (Some("host"), _) => st.start_host(),
                        (Some("client"), Some(c)) => st.connect(&c),
                        _ => Ok(()),
                    };
                    if let Err(e) = r {
                        log::warn!("could not resume: {e}");
                    }
                });
            }

            // Background: place new devices, keep audio and tray text current.
            {
                let state = state.clone();
                let handle = app.handle().clone();
                std::thread::spawn(move || loop {
                    let devices = state.host.lock().unwrap().as_ref().map(|h| h.handle.reg.devices());
                    if let Some(d) = devices {
                        state.auto_place(&d);
                    }
                    state.kick_audio();
                    // Updates: check shortly after launch, then every 6 h; install when idle.
                    if state.settings.lock().unwrap().auto_update {
                        let (due, ready) = {
                            let u = state.upd.lock().unwrap();
                            let due = !u.checking
                                && !u.installing
                                && match u.last_check {
                                    None => state.started.elapsed() > Duration::from_secs(15),
                                    Some(t) => t.elapsed() > Duration::from_secs(6 * 3600),
                                };
                            (due, u.available.is_some() && !u.installing && u.error.is_none())
                        };
                        if due {
                            state.check_update_async();
                        } else if ready && !state.in_use() {
                            state.install_update_async(handle.clone());
                        }
                    }
                    let text = state.summary();
                    if let Some(item) = state.tray_status.lock().unwrap().as_ref() {
                        let _ = item.set_text(&text);
                    }
                    if let Some(tray) = handle.tray_by_id("main") {
                        let _ = tray.set_tooltip(Some(format!("glide: {text}")));
                    }
                    std::thread::sleep(Duration::from_millis(800));
                });
            }

            if !launched_by_autostart && !start_min {
                show_main(app.handle());
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            let st = window.app_handle().state::<Arc<App>>();
            let (close_to_tray, min_to_tray) = {
                let s = st.settings.lock().unwrap();
                (s.close_to_tray, s.minimize_to_tray)
            };
            match event {
                WindowEvent::CloseRequested { api, .. } => {
                    api.prevent_close();
                    if close_to_tray {
                        let _ = window.hide();
                    } else {
                        st.stop_all();
                        window.app_handle().exit(0);
                    }
                }
                WindowEvent::Resized(_) => {
                    if min_to_tray && window.is_minimized().unwrap_or(false) {
                        let _ = window.hide();
                    }
                }
                _ => {}
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running glide");
}

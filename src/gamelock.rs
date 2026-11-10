//! "Stay on this screen during games."
//!
//! A full-screen game shouldn't lose the cursor to another PC when the mouse
//! hits an edge, but a full-screen movie or browser should behave normally.
//! The foreground window must cover its monitor AND show a game signal:
//!   - it has confined the cursor (games do; video players and browsers don't)
//!   - Windows Game Bar recognises its exe as a game
//!   - its exe lives in a game library folder (Steam, Epic, Riot, Xbox, ...)
//!   - its process refuses inspection (anti-cheat protected, e.g. VALORANT)
//! Browsers and media players are never treated as games, and the user can
//! override any app either way.

use std::collections::HashMap;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, serde::Serialize)]
pub struct Foreground {
    /// Override key: lowercase exe file name (or window title if the process is protected).
    pub app: String,
    pub title: String,
    pub fullscreen: bool,
    pub game: bool,
    pub reason: &'static str,
}

static OVERRIDES: RwLock<Option<HashMap<String, bool>>> = RwLock::new(None);
static CACHE: Mutex<Option<(Instant, bool)>> = Mutex::new(None);
static RECENT: Mutex<Vec<Foreground>> = Mutex::new(Vec::new());

pub fn set_overrides(map: HashMap<String, bool>) {
    *OVERRIDES.write().unwrap() = Some(map);
    *CACHE.lock().unwrap() = None;
}

fn override_for(app: &str) -> Option<bool> {
    OVERRIDES.read().unwrap().as_ref().and_then(|m| m.get(app).copied())
}

/// Full-screen apps seen recently (newest first), for the settings list.
pub fn recent() -> Vec<Foreground> {
    RECENT.lock().unwrap().clone()
}

fn remember(fg: &Foreground) {
    if !fg.fullscreen {
        return;
    }
    let mut r = RECENT.lock().unwrap();
    r.retain(|x| x.app != fg.app);
    r.insert(0, fg.clone());
    r.truncate(8);
}

/// Inspect the foreground window now.
pub fn check() -> Option<Foreground> {
    let fg = imp::foreground()?;
    remember(&fg);
    Some(fg)
}

/// Should edge switching be blocked right now? Cached briefly: it's called on edge hits.
pub fn blocking() -> bool {
    if !crate::flags::on(&crate::flags::GAME_LOCK) {
        return false;
    }
    let mut c = CACHE.lock().unwrap();
    if let Some((at, v)) = *c {
        if at.elapsed() < Duration::from_millis(250) {
            return v;
        }
    }
    let v = check().map(|f| f.game).unwrap_or(false);
    *c = Some((Instant::now(), v));
    v
}

/// Browsers, video players and other apps people run full screen that aren't games.
const NOT_GAMES: &[&str] = &[
    "chrome.exe", "msedge.exe", "firefox.exe", "brave.exe", "opera.exe", "opera_gx.exe", "vivaldi.exe", "arc.exe",
    "iexplore.exe", "vlc.exe", "mpv.exe", "mpc-hc.exe", "mpc-hc64.exe", "mpc-be.exe", "mpc-be64.exe",
    "potplayer.exe", "potplayermini.exe", "potplayermini64.exe", "wmplayer.exe", "video.ui.exe",
    "microsoft.media.player.exe", "kodi.exe", "plex.exe", "plex htpc.exe", "jellyfin media player.exe",
    "stremio.exe", "spotify.exe", "powerpnt.exe", "explorer.exe", "applicationframehost.exe",
    "mstsc.exe", "msrdc.exe", "obs64.exe", "teams.exe", "ms-teams.exe", "zoom.exe", "code.exe",
    "windowsterminal.exe", "glide.exe", "steamwebhelper.exe",
];

/// Folders game launchers install into.
const GAME_DIRS: &[&str] = &[
    "\\steamapps\\common\\", "\\epic games\\", "\\riot games\\", "\\xboxgames\\", "\\gog galaxy\\games\\",
    "\\gog games\\", "\\ea games\\", "\\origin games\\", "\\ubisoft game launcher\\games\\", "\\battle.net\\",
    "\\rockstar games\\", "\\games\\",
];

fn classify(app: &str, path: &str, fullscreen: bool, clipped: bool, protected: bool, gamebar: bool) -> (bool, &'static str) {
    if !fullscreen {
        return (false, "not full screen");
    }
    if let Some(v) = override_for(app) {
        return (v, if v { "marked as a game" } else { "marked as not a game" });
    }
    if NOT_GAMES.contains(&app) {
        return (false, "browser or media app");
    }
    if clipped {
        return (true, "captures the mouse");
    }
    if protected {
        return (true, "protected by anti-cheat");
    }
    if gamebar {
        return (true, "known to Windows Game Bar");
    }
    let p = path.to_lowercase();
    if GAME_DIRS.iter().any(|d| p.contains(d)) {
        return (true, "installed in a game library");
    }
    (false, "full-screen app")
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::collections::HashSet;
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::Foundation::*;
    use windows_sys::Win32::Graphics::Gdi::*;
    use windows_sys::Win32::System::Registry::*;
    use windows_sys::Win32::System::Threading::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    static GAMEBAR: Mutex<Option<(Instant, HashSet<String>)>> = Mutex::new(None);

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Exe paths Windows Game Bar has identified as games (refreshed each minute).
    fn gamebar_has(path: &str) -> bool {
        if path.is_empty() {
            return false;
        }
        let mut g = GAMEBAR.lock().unwrap();
        let stale = g.as_ref().map(|(t, _)| t.elapsed() > Duration::from_secs(60)).unwrap_or(true);
        if stale {
            *g = Some((Instant::now(), unsafe { read_gamebar() }));
        }
        g.as_ref().map(|(_, s)| s.contains(&path.to_lowercase())).unwrap_or(false)
    }

    unsafe fn read_gamebar() -> HashSet<String> {
        let mut out = HashSet::new();
        let root = wide("System\\GameConfigStore\\Children");
        let mut key: HKEY = std::ptr::null_mut();
        if RegOpenKeyExW(HKEY_CURRENT_USER, root.as_ptr(), 0, KEY_READ, &mut key) != 0 {
            return out;
        }
        let value = wide("MatchedExeFullPath");
        for i in 0.. {
            let mut name = [0u16; 256];
            let mut len = name.len() as u32;
            let rc = RegEnumKeyExW(key, i, name.as_mut_ptr(), &mut len, std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
            if rc != 0 {
                break;
            }
            let mut buf = [0u16; 1024];
            let mut size = (buf.len() * 2) as u32;
            if RegGetValueW(key, name.as_ptr(), value.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr() as _, &mut size) == 0 {
                let n = (size as usize / 2).saturating_sub(1);
                out.insert(String::from_utf16_lossy(&buf[..n]).to_lowercase());
            }
        }
        RegCloseKey(key);
        out
    }

    pub fn foreground() -> Option<Foreground> {
        unsafe {
            let h = GetForegroundWindow();
            if h.is_null() || h == GetShellWindow() {
                return None;
            }
            let mut cls = [0u16; 64];
            let n = GetClassNameW(h, cls.as_mut_ptr(), cls.len() as i32).max(0) as usize;
            let cls = String::from_utf16_lossy(&cls[..n]);
            if matches!(cls.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd") {
                return None;
            }
            let mut title = [0u16; 256];
            let n = GetWindowTextW(h, title.as_mut_ptr(), title.len() as i32).max(0) as usize;
            let title = String::from_utf16_lossy(&title[..n]);

            // Covers its whole monitor?
            let mut r: RECT = zeroed();
            GetWindowRect(h, &mut r);
            let mon = MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST);
            let mut mi: MONITORINFO = zeroed();
            mi.cbSize = size_of::<MONITORINFO>() as u32;
            GetMonitorInfoW(mon, &mut mi);
            let m = mi.rcMonitor;
            let covers = r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom;
            // A maximized window with a title bar is not "full screen" (it can cover the
            // monitor when the taskbar auto-hides). Games, F11 and video have no caption.
            let style = GetWindowLongW(h, GWL_STYLE) as u32;
            let captioned = style & WS_CAPTION == WS_CAPTION;
            let fullscreen = covers && !(captioned && IsZoomed(h) != 0);

            // Which program?
            let mut pid = 0u32;
            GetWindowThreadProcessId(h, &mut pid);
            let ph = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            let (path, protected) = if ph.is_null() {
                (String::new(), GetLastError() == ERROR_ACCESS_DENIED)
            } else {
                let mut buf = [0u16; 1024];
                let mut len = buf.len() as u32;
                let ok = QueryFullProcessImageNameW(ph, 0, buf.as_mut_ptr(), &mut len);
                CloseHandle(ph);
                (if ok != 0 { String::from_utf16_lossy(&buf[..len as usize]) } else { String::new() }, false)
            };
            let app = match path.rsplit('\\').next().filter(|s| !s.is_empty()) {
                Some(f) => f.to_lowercase(),
                None => {
                    if title.is_empty() {
                        "protected app".into()
                    } else {
                        title.clone()
                    }
                }
            };

            // Cursor confined to a smaller area than the whole desktop?
            let mut clip: RECT = zeroed();
            GetClipCursor(&mut clip);
            let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
            let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
            let clipped = clip.left > vx || clip.top > vy || clip.right < vx + vw || clip.bottom < vy + vh;

            let gamebar = gamebar_has(&path);
            let (game, reason) = classify(&app, &path, fullscreen, clipped, protected, gamebar);
            Some(Foreground { app, title, fullscreen, game, reason })
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;
    pub fn foreground() -> Option<Foreground> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification() {
        // args: app, path, fullscreen, mouse captured, anti-cheat protected, Game Bar knows it
        // A full-screen browser or video player is never a game, even if it captures the mouse.
        assert!(!classify("chrome.exe", "C:\\Program Files\\Google\\chrome.exe", true, true, false, false).0);
        assert!(!classify("vlc.exe", "C:\\vlc\\vlc.exe", true, true, false, false).0);
        // Windowed apps never lock.
        assert!(!classify("game.exe", "D:\\SteamLibrary\\steamapps\\common\\x\\game.exe", false, true, true, true).0);
        // Each signal on its own is enough.
        assert!(classify("x.exe", "", true, true, false, false).0);
        assert!(classify("protected app", "", true, false, true, false).0);
        assert!(classify("x.exe", "C:\\x.exe", true, false, false, true).0);
        assert!(classify("eldenring.exe", "D:\\SteamLibrary\\steamapps\\common\\ELDEN RING\\eldenring.exe", true, false, false, false).0);
        // A plain full-screen app with no signal stays unlocked.
        assert!(!classify("notepad.exe", "C:\\Windows\\notepad.exe", true, false, false, false).0);

        // Overrides beat detection. Kept in this same test because overrides are
        // global: a separate test running in parallel would race them.
        set_overrides(HashMap::from([("chrome.exe".to_string(), true), ("eldenring.exe".to_string(), false)]));
        assert!(classify("chrome.exe", "", true, false, false, false).0);
        assert!(!classify("eldenring.exe", "D:\\steamapps\\common\\e\\eldenring.exe", true, true, true, false).0);
        set_overrides(HashMap::new());
    }
}

//! Self-update from GitHub releases.
//!
//! A release carries `glide.exe` plus `glide.exe.sig`, an Ed25519 signature
//! made with the maintainer's offline key (`glide release-sign`). The app only
//! installs a binary whose signature verifies against `PUBLIC_KEY` below, so a
//! compromised download or GitHub account can't push code to users.
//!
//! Install: download next to the running exe, verify, rename the running exe
//! to `glide.exe.old` (Windows allows renaming a running image), move the new
//! one into place, then relaunch with `--after-update`. The `.old` file is
//! deleted on the next start.

use base64::{engine::general_purpose::STANDARD, Engine};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub const REPO: &str = "d3osaju/glide";
const SIG_PREFIX: &str = "glide-sig-v1:";
const MAX_DOWNLOAD: u64 = 200 * 1024 * 1024;

/// Release-signing public key (private half lives only on the maintainer's PC).
pub const PUBLIC_KEY: [u8; 32] = [
    5, 238, 215, 81, 112, 88, 228, 120, 180, 50, 102, 72, 118, 36, 32, 122, 167, 14, 12, 47, 181, 197, 137, 132, 236, 233,
    121, 150, 224, 163, 77, 247,
];

#[derive(Clone, serde::Serialize)]
pub struct Release {
    pub version: String,
    pub notes: String,
    pub url: String,
    #[serde(skip)]
    exe_url: String,
    #[serde(skip)]
    sig_url: String,
    pub size: u64,
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn asset_name() -> Option<&'static str> {
    if cfg!(all(windows, target_arch = "x86_64")) {
        Some("glide.exe")
    } else {
        None
    }
}

fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim().trim_start_matches('v');
    let core = v.split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
    Some((it.next()??, it.next().flatten().unwrap_or(0), it.next().flatten().unwrap_or(0)))
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

fn agent() -> Result<ureq::Agent, String> {
    let tls = native_tls::TlsConnector::new().map_err(|e| e.to_string())?;
    Ok(ureq::AgentBuilder::new()
        .tls_connector(Arc::new(tls))
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(60))
        .user_agent(concat!("glide-updater/", env!("CARGO_PKG_VERSION")))
        .build())
}

/// Latest release if it's newer than this build and has an asset for this OS.
pub fn check() -> Result<Option<Release>, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let v: serde_json::Value = agent()?
        .get(&url)
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("couldn't reach GitHub: {e}"))?
        .into_json()
        .map_err(|e| e.to_string())?;
    let tag = v["tag_name"].as_str().ok_or("release has no tag")?;
    if !is_newer(tag, current_version()) {
        return Ok(None);
    }
    let Some(name) = asset_name() else {
        return Err(format!("{tag} is out, but there's no automatic update for this OS yet"));
    };
    let assets = v["assets"].as_array().cloned().unwrap_or_default();
    let find = |n: &str| assets.iter().find(|a| a["name"].as_str() == Some(n)).cloned();
    let exe = find(name).ok_or_else(|| format!("{tag} has no {name}"))?;
    let sig = find(&format!("{name}.sig")).ok_or_else(|| format!("{tag} isn't signed, so it won't be installed"))?;
    Ok(Some(Release {
        version: tag.trim_start_matches('v').to_string(),
        notes: v["body"].as_str().unwrap_or("").chars().take(4000).collect(),
        url: v["html_url"].as_str().unwrap_or("").to_string(),
        exe_url: exe["browser_download_url"].as_str().unwrap_or("").to_string(),
        sig_url: sig["browser_download_url"].as_str().unwrap_or("").to_string(),
        size: exe["size"].as_u64().unwrap_or(0),
    }))
}

fn download(agent: &ureq::Agent, url: &str, progress: &dyn Fn(u64)) -> Result<Vec<u8>, String> {
    let resp = agent.get(url).call().map_err(|e| format!("download failed: {e}"))?;
    let mut r = resp.into_reader().take(MAX_DOWNLOAD);
    let mut out = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = r.read(&mut buf).map_err(|e| format!("download failed: {e}"))?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        progress(out.len() as u64);
    }
    Ok(out)
}

pub fn verify(data: &[u8], sig_text: &str, key: &[u8; 32]) -> Result<(), String> {
    let b64 = sig_text.trim().strip_prefix(SIG_PREFIX).ok_or("unrecognised signature format")?;
    let bytes: [u8; 64] = STANDARD
        .decode(b64)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or("malformed signature")?;
    let vk = VerifyingKey::from_bytes(key).map_err(|_| "this build has no valid update key")?;
    vk.verify(data, &Signature::from_bytes(&bytes)).map_err(|_| "signature check FAILED: update rejected".to_string())
}

/// Download, verify and swap in the new binary. Returns the path to launch.
pub fn install(rel: &Release, progress: &dyn Fn(u64)) -> Result<PathBuf, String> {
    let agent = agent()?;
    let sig = String::from_utf8(download(&agent, &rel.sig_url, &|_| {})?).map_err(|_| "malformed signature file")?;
    let data = download(&agent, &rel.exe_url, progress)?;
    verify(&data, &sig, &PUBLIC_KEY)?;

    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let new = exe.with_extension("exe.new");
    let old = exe.with_extension("exe.old");
    match std::fs::write(&new, &data) {
        Ok(()) => {
            let _ = std::fs::remove_file(&old);
            std::fs::rename(&exe, &old).map_err(|e| format!("couldn't replace glide.exe: {e}"))?;
            if let Err(e) = std::fs::rename(&new, &exe) {
                let _ = std::fs::rename(&old, &exe);
                return Err(format!("couldn't replace glide.exe: {e}"));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => elevated_swap(&exe, &data)?,
        Err(e) => return Err(format!("couldn't write update: {e}")),
    }
    Ok(exe)
}

/// For installs in protected folders (e.g. Program Files): one UAC prompt.
#[cfg(windows)]
fn elevated_swap(exe: &Path, data: &[u8]) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    let tmp = std::env::temp_dir().join(format!("glide-update-{}.exe", std::process::id()));
    std::fs::write(&tmp, data).map_err(|e| e.to_string())?;
    let q = |p: &Path| p.display().to_string().replace('\'', "''");
    let old = exe.with_extension("exe.old");
    let script = format!(
        "Remove-Item -Force -ErrorAction SilentlyContinue '{old}'; Move-Item -Force '{exe}' '{old}'; Move-Item -Force '{tmp}' '{exe}'",
        old = q(&old),
        exe = q(exe),
        tmp = q(&tmp)
    );
    let outer = format!(
        "Start-Process powershell -Verb RunAs -Wait -WindowStyle Hidden -ArgumentList '-NoProfile','-Command',\"{}\"",
        script.replace('"', "`\"")
    );
    let status = std::process::Command::new("powershell")
        .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &outer])
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .status()
        .map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&tmp);
    if !status.success() || !old.exists() {
        return Err("update needs administrator permission (prompt was declined)".into());
    }
    Ok(())
}

#[cfg(not(windows))]
fn elevated_swap(_exe: &Path, _data: &[u8]) -> Result<(), String> {
    Err("no permission to replace the app".into())
}

/// Start the freshly installed binary; the caller then exits.
pub fn relaunch(exe: &Path, to_tray: bool) -> Result<(), String> {
    let mut c = std::process::Command::new(exe);
    c.arg("--after-update");
    if to_tray {
        c.arg("--tray");
    }
    c.spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// Remove the previous version left behind by an update.
pub fn cleanup() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(exe.with_extension("exe.old"));
        let _ = std::fs::remove_file(exe.with_extension("exe.new"));
    }
}

// ---- maintainer tools (hidden CLI) ----

pub fn keygen(out: &Path) -> Result<(), String> {
    if out.exists() {
        return Err(format!("{} already exists; refusing to overwrite a signing key", out.display()));
    }
    let sk = SigningKey::generate(&mut rand::rngs::OsRng);
    if let Some(d) = out.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    std::fs::write(out, STANDARD.encode(sk.to_bytes())).map_err(|e| e.to_string())?;
    let pk = sk.verifying_key().to_bytes();
    println!("Private key written to {} (back it up; never commit it).", out.display());
    println!("Paste into src/update.rs:\npub const PUBLIC_KEY: [u8; 32] = {:?};", pk);
    Ok(())
}

pub fn sign(file: &Path, key: &Path) -> Result<PathBuf, String> {
    let kb: [u8; 32] = STANDARD
        .decode(std::fs::read_to_string(key).map_err(|e| format!("{}: {e}", key.display()))?.trim())
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or("signing key file is malformed")?;
    let sk = SigningKey::from_bytes(&kb);
    if sk.verifying_key().to_bytes() != PUBLIC_KEY {
        return Err("this signing key doesn't match PUBLIC_KEY in src/update.rs".into());
    }
    let data = std::fs::read(file).map_err(|e| e.to_string())?;
    let sig = sk.sign(&data);
    let text = format!("{SIG_PREFIX}{}\n", STANDARD.encode(sig.to_bytes()));
    verify(&data, &text, &PUBLIC_KEY)?;
    let out = PathBuf::from(format!("{}.sig", file.display()));
    std::fs::write(&out, text).map_err(|e| e.to_string())?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(is_newer("v0.3.0", "0.2.0"));
        assert!(is_newer("v1.0", "0.9.9"));
        assert!(!is_newer("v0.2.0", "0.2.0"));
        assert!(!is_newer("v0.1.9", "0.2.0"));
        assert!(!is_newer("garbage", "0.2.0"));
    }

    #[test]
    fn sign_and_verify() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let pk = sk.verifying_key().to_bytes();
        let data = b"binary";
        let text = format!("{SIG_PREFIX}{}", STANDARD.encode(sk.sign(data).to_bytes()));
        assert!(verify(data, &text, &pk).is_ok());
        assert!(verify(b"tampered", &text, &pk).is_err());
        assert!(verify(data, &text, &[1u8; 32]).is_err());
    }
}

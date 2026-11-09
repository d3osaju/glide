//! Windows audio endpoint helpers: mute/unmute a playback device while its
//! sound is being streamed elsewhere (WASAPI loopback keeps capturing, the
//! local speakers go quiet).

use windows::core::PWSTR;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{eConsole, eRender, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE};
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;

fn enumerator() -> windows::core::Result<IMMDeviceEnumerator> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
    }
}

fn friendly_name(d: &IMMDevice) -> Option<String> {
    unsafe {
        let store = d.OpenPropertyStore(STGM_READ).ok()?;
        let v = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
        let p: PWSTR = PropVariantToStringAlloc(&v).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as _));
        s
    }
}

/// The playback endpoint named `name` (cpal's device name == Windows friendly
/// name), or the default playback device when `name` is None.
fn render_device(name: Option<&str>) -> windows::core::Result<IMMDevice> {
    let e = enumerator()?;
    unsafe {
        if let Some(name) = name {
            let col = e.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
            for i in 0..col.GetCount()? {
                let d = col.Item(i)?;
                if friendly_name(&d).as_deref() == Some(name) {
                    return Ok(d);
                }
            }
        }
        e.GetDefaultAudioEndpoint(eRender, eConsole)
    }
}

fn volume(name: Option<&str>) -> windows::core::Result<IAudioEndpointVolume> {
    unsafe { render_device(name)?.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) }
}

/// Marker so a crash or kill while muted is undone on the next launch.
fn marker() -> std::path::PathBuf {
    std::env::temp_dir().join("glide-muted-device.txt")
}

/// Mutes the device and returns a guard that restores its previous mute state.
pub fn mute(name: Option<&str>) -> Result<MuteGuard, String> {
    let vol = volume(name).map_err(|e| format!("can't access playback device: {e}"))?;
    unsafe {
        let was = vol.GetMute().map_err(|e| e.to_string())?.as_bool();
        if !was {
            let _ = std::fs::write(marker(), name.unwrap_or(""));
        }
        vol.SetMute(true, std::ptr::null()).map_err(|e| e.to_string())?;
        Ok(MuteGuard { name: name.map(str::to_string), was_muted: was })
    }
}

/// Undo a mute left behind by a previous run that didn't exit cleanly.
pub fn recover() {
    if let Ok(name) = std::fs::read_to_string(marker()) {
        unmute(if name.is_empty() { None } else { Some(&name) });
        let _ = std::fs::remove_file(marker());
    }
}

pub struct MuteGuard {
    name: Option<String>,
    was_muted: bool,
}

impl Drop for MuteGuard {
    fn drop(&mut self) {
        if self.was_muted {
            return;
        }
        let _ = std::fs::remove_file(marker());
        // Re-resolve: the COM pointer may belong to a thread that's gone.
        if let Ok(v) = volume(self.name.as_deref()) {
            unsafe {
                let _ = v.SetMute(false, std::ptr::null());
            }
        }
    }
}

/// Unmutes a device (used to recover if glide died while muting it).
pub fn unmute(name: Option<&str>) {
    if let Ok(v) = volume(name) {
        unsafe {
            let _ = v.SetMute(false, std::ptr::null());
        }
    }
}


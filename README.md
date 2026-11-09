# glide

**Website:** https://glide.zetalabs.in · **Download:** [latest release](https://github.com/d3osaju/glide/releases/latest)

Share one keyboard and mouse across several PCs and Macs on your network.
Push the cursor off a screen edge and it continues on the next device. glide
also syncs the clipboard and can stream sound or a microphone between devices.
It's a small native Rust app (one `glide.exe`) that sits in the tray.

## Install (each Windows PC)

1. Download `glide-windows-x64.zip` from the
   [latest release](https://github.com/d3osaju/glide/releases/latest) and unzip it.
2. Double-click **`Install glide.cmd`** and accept the admin prompt.
   This installs glide to `%LOCALAPPDATA%\Programs\glide`, adds Start Menu and
   desktop shortcuts, allows glide through the firewall on Private networks,
   adds the network-priority policy, and opens the app.

**Updates install themselves.** glide checks GitHub at launch and every 6
hours. It installs a new version only while you aren't using another device,
then restarts and resumes your session. Each update must carry a valid
signature from the release key, or glide refuses it. You can also use
*Settings → Updates → Check now*, or turn automatic updates off.

> Upgrading from v0.2.0? That version had no updater: install v0.3.0 once with
> the installer, and every later version arrives automatically.

To remove glide, run `uninstall.ps1`. Add `-Purge` to also delete its settings.

> Your network must be set to **Private** in Windows (Settings → Network &
> internet → your network). The installer warns you if it's Public.

## Use

When glide opens, choose one of two options.

**Start hosting.** Do this on the PC whose keyboard and mouse you use. glide
shows a **connection code** with a *Copy code* button, the PC's addresses, and
a live **screen layout** map. Each connected device appears on the map. Set
which side of which screen it sits on (Right of / Left of / Above / Below).
Devices can also chain off each other.

**Connect to a host.** Do this on every other device. Hosts on your network
appear automatically. Paste the host's code once and press **Connect**; after
that, one click reconnects to that host.

- **Ctrl+Alt+L** locks the cursor to the current screen until you press it again.
- Move the cursor off the shared edge to cross over. **Ctrl+Alt+Esc** brings
  it back to the host from anywhere.
- Closing or minimizing the window sends glide to the **tray**. Left-click
  the tray icon to open it. Right-click it for *Stop sharing* or *Quit*.
- If the cursor's device disconnects, the cursor returns to the host, and any
  held keys or buttons are released on that device.

## Settings (every option has an on/off switch, applied instantly)

| Group | Option | Default |
|---|---|---|
| Sharing | Sync clipboard | on |
| | Swap Ctrl and Cmd (Mac driven from a PC keyboard) | off (on for Mac) |
| | Don't switch screens while dragging | on |
| | Ctrl+Alt+Esc return hotkey | on |
| | Visible on the network (host discovery) | on |
| Games | Stay on screen during games (full-screen games keep the cursor; videos and browsers don't) | on |
| | Per-app override: Auto / Game / Not a game, from the list of recent full-screen apps | Auto |
| Network | Priority traffic (DSCP marking, anti-torrent lag) | on |
| | Wi-Fi keepalive | on |
| App | Start with Windows (opens in the tray) | off |
| | Resume last session on launch | on |
| | Start minimized | off |
| | Close to tray / Minimize to tray | on |
| Audio | Hear the other device here (output follows your current default, e.g. headphones) | off |
| | Send to the other device (microphone, this PC's sound, or any input) | off |
| | Mute this PC while sending its sound | on |
| Updates | Update automatically | on |

**Audio both ways at once** (a typical setup):

| | Host (the PC you sit at, with headphones) | Remote |
|---|---|---|
| Hear | **Hear the remote here**: plays on your headphones | **Hear the host here** → Output: *CABLE Input* (virtual mic) |
| Send | **Send to the remote**, Source: *Microphone* | **Send to the host**, Source: *This PC's sound* + **Mute this PC while sending** |

The remote's sound plays on your headphones and the remote stays silent. Your
host microphone shows up on the remote as the *CABLE Output* mic for calls.
glide refuses to play received audio into the same output it's sending, which
would echo back, and tells you to pick another output.

**Use a received stream as a microphone:** install the free
[VB-Audio Virtual Cable](https://vb-audio.com/Cable/), set **Output** to
"CABLE Input", and pick "CABLE Output" as the mic in your apps. On a Mac, use
[BlackHole](https://existential.audio/blackhole/).

**Microphone blocked:** if Windows denies microphone access, go to *Settings
→ Privacy & security → Microphone* and turn on **Let desktop apps access your
microphone**.

## Why it doesn't lag

| Where lag usually comes from | What glide does |
|---|---|
| Event queues and thread hops | The mouse hook encrypts each move and sends it straight out on UDP, and the receiver injects it immediately. Measured loopback latency, with encryption: **14 µs median, 39 µs p99**. |
| A lost packet waiting for a TCP retransmit (300 ms or more on Windows) | Mouse moves are absolute positions, so the next packet corrects any loss. Keys, clicks and scroll go out as two UDP copies plus a TCP backup, and each is applied once, in order, from whichever copy arrives first. |
| Torrents or downloads filling the router queue | Packets are marked DSCP 46 (Expedited Forwarding), so routers and the Wi-Fi WMM voice queue send them first. The installer adds the Windows QoS policy that makes the OS honour this. |
| Wi-Fi power saving | While a device has the cursor, it sends a 1-byte keepalive every 20 ms so its radio stays awake. |
| CPU contention | glide runs at high process priority, and its input threads use `TIME_CRITICAL` priority. |

glide traffic stays on your LAN, so a saturated internet download on a cable
doesn't affect it. If you still see lag on Wi-Fi with a torrent running, cap
the torrent's upload at about 80% of your line speed.

## Security

Everything, including every keystroke, is encrypted and authenticated with
ChaCha20-Poly1305. Each session derives fresh keys from the host's 32-byte key.
The connection code **contains that key**, so share it only with your own
devices. If a code leaks, press **New code**: devices using the old code can no
longer connect. The network discovery broadcast carries only the host's name,
port and a short key fingerprint, never the key.

## Command line (optional)

The same `glide.exe` also runs headless when given arguments:

```
glide genkey
glide server --config glide.toml          # see glide.example.toml
glide client --server 192.168.1.10:24800 --name laptop --key <key>
glide audio-devices | audio-send | audio-recv
```

## Ports

| Port | Use |
|---|---|
| TCP 24800 (host) | Control, keys, clipboard |
| UDP, ephemeral | Mouse moves and fast keys |
| UDP 24801 | Host discovery broadcast |
| UDP 24810 | Audio |

## Publishing an update (maintainer)

```powershell
.
elease.ps1 -Version 0.3.1 -Notes "What changed"
```

This bumps the version, checks the UI script, runs the tests, builds, signs
`glide.exe` with the private key at `%USERPROFILE%\.glide
elease-signing.key`,
then tags, pushes and publishes the GitHub release. Installed copies pick it up
automatically. Keep the key backed up and private: without it you can't
publish updates, and anyone who has it can.

## Build

```powershell
.\build-dist.ps1      # -> dist\ (glide.exe, Install glide.cmd, zip)
cargo test --release  # includes an end-to-end loopback latency test
```

**macOS:** build on the Mac with `cargo build --release`. The Mac GUI can't be
cross-compiled from Windows, and the macOS input code has not been tested on a
real Mac yet. Grant **Accessibility** and **Input Monitoring** in System
Settings → Privacy & Security.

## Known limits

- Ctrl+Alt+Del and the Windows secure desktop (UAC prompts, lock screen) can't
  be captured or injected. This applies to all tools of this kind.
- To control elevated (admin) windows on a client, glide must run as
  administrator on that client.
- Games that read raw mouse input on a client get absolute moves, which are
  fine for desktop use but not for mouse-look in shooters.
- Linux isn't supported yet.

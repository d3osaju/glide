// Release builds are a GUI app (no console window). CLI subcommands still
// work: they attach to the parent terminal's console.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod audio;
mod client;
mod clip;
mod config;
mod crypto;
mod flags;
mod gui;
mod keymap;
mod net;
mod pair;
mod platform;
mod proto;
mod server;

#[cfg(test)]
mod tests;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "glide", version, about = "Share one keyboard, mouse, clipboard and audio across machines on your LAN")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
    /// Log verbosity: error, warn, info, debug
    #[arg(long, global = true, default_value = "info")]
    log: String,
}

#[derive(Subcommand)]
enum Cmd {
    /// Generate a random shared key. Use the same key on every machine.
    Genkey,
    /// Run on the machine whose keyboard and mouse you want to share.
    Server {
        /// Path to the server config (see glide.example.toml)
        #[arg(short, long, default_value = "glide.toml")]
        config: String,
        #[arg(long, env = "GLIDE_KEY", hide_env_values = true)]
        key: Option<String>,
    },
    /// Run on each machine that should be controlled.
    Client {
        /// Server address, e.g. 192.168.1.10:24800
        #[arg(short, long)]
        server: String,
        /// This screen's name; must match a name in the server's [screens] layout
        #[arg(short, long)]
        name: String,
        #[arg(long, env = "GLIDE_KEY", hide_env_values = true)]
        key: String,
        /// Swap Ctrl and Cmd/Win (use on a Mac controlled from a PC keyboard)
        #[arg(long)]
        swap_ctrl_cmd: bool,
        #[arg(long)]
        no_clipboard: bool,
    },
    /// List audio input and output devices.
    AudioDevices,
    /// Stream audio from this machine to another (system output, mic, or a named device).
    AudioSend {
        /// Receiver address, e.g. 192.168.1.20:24810
        #[arg(long)]
        to: String,
        /// "loopback" (what this PC is playing, Windows), "mic", or a device-name substring
        #[arg(long, default_value = "loopback")]
        source: String,
        #[arg(long, env = "GLIDE_KEY", hide_env_values = true)]
        key: String,
    },
    /// Play audio streamed from another machine.
    AudioRecv {
        #[arg(long, default_value = "0.0.0.0:24810")]
        listen: String,
        /// Output device-name substring (default output if omitted). Use a virtual cable to expose it as a mic.
        #[arg(long)]
        output: Option<String>,
        /// Jitter buffer in milliseconds. Lower = less delay, higher = fewer dropouts on Wi-Fi.
        #[arg(long, default_value_t = 30)]
        latency_ms: u32,
        #[arg(long, env = "GLIDE_KEY", hide_env_values = true)]
        key: String,
    },
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let gui = args.is_empty() || args.iter().all(|a| a == "--tray");
    platform::init_process();
    if gui {
        env_logger::Builder::new().parse_filters("info").format_target(false).init();
        gui::run(!args.is_empty());
        platform::restore_cursor();
        return;
    }
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::System::Console::AttachConsole(windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS);
    }
    let cli = Cli::parse();
    env_logger::Builder::new()
        .parse_filters(&cli.log)
        .format_target(false)
        .init();

    // Never leave the cursor hidden on exit or crash.
    let _ = ctrlc::set_handler(|| {
        platform::restore_cursor();
        std::process::exit(0);
    });
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        platform::restore_cursor();
        prev(info);
    }));

    if let Err(e) = run(cli.cmd) {
        platform::restore_cursor();
        log::error!("{e}");
        std::process::exit(1);
    }
}

fn run(cmd: Cmd) -> Result<(), String> {
    match cmd {
        Cmd::Genkey => {
            println!("{}", crypto::gen_key());
            Ok(())
        }
        Cmd::Server { config, key } => {
            let cfg = config::ServerConfig::load(&config)?;
            let key = key
                .or_else(|| cfg.key.clone())
                .ok_or("no key: set `key` in the config, pass --key, or set GLIDE_KEY")?;
            let psk = crypto::parse_key(&key)?;
            server::run(cfg, psk)
        }
        Cmd::Client { server, name, key, swap_ctrl_cmd, no_clipboard } => {
            let key = crypto::parse_key(&key)?;
            client::run(client::Options { server, name, key, swap_ctrl_cmd, clipboard: !no_clipboard }, client::Ctl::new());
            Ok(())
        }
        Cmd::AudioDevices => audio::list_devices(),
        Cmd::AudioSend { to, source, key } => audio::send(&to, &source, crypto::parse_key(&key)?),
        Cmd::AudioRecv { listen, output, latency_ms, key } => {
            audio::recv(&listen, output.as_deref(), latency_ms.clamp(5, 1000), crypto::parse_key(&key)?)
        }
    }
}

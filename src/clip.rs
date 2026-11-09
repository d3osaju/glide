//! Text clipboard sync. Clipboard reads can take milliseconds, so they never
//! run on the input hook thread: the router posts jobs to this worker.
//!
//! Flow: when the cursor leaves the server for a client, the server pushes its
//! clipboard to that client. When the cursor leaves a client, the client sends
//! its clipboard back; the server applies it locally and forwards it to
//! whichever client is active now (for client -> client hops).

use crate::proto::{Msg, MAX_CLIPBOARD};
use crate::server::{Client, Registry};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::mpsc;
use std::sync::Arc;

pub enum Job {
    PushTo(Arc<Client>),
    FromRemote { from: String, text: String },
}

pub fn digest(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

pub fn read_text(cb: &mut Option<arboard::Clipboard>) -> Option<String> {
    if cb.is_none() {
        *cb = arboard::Clipboard::new().ok();
    }
    let t = cb.as_mut()?.get_text().ok()?;
    (!t.is_empty() && t.len() <= MAX_CLIPBOARD).then_some(t)
}

pub fn write_text(cb: &mut Option<arboard::Clipboard>, text: &str) {
    if cb.is_none() {
        *cb = arboard::Clipboard::new().ok();
    }
    if let Some(cb) = cb.as_mut() {
        if let Err(e) = cb.set_text(text.to_string()) {
            log::warn!("clipboard write failed: {e}");
        }
    }
}

pub fn spawn_server_worker(reg: Arc<Registry>) -> mpsc::Sender<Job> {
    let (tx, rx) = mpsc::channel::<Job>();
    std::thread::Builder::new()
        .name("clipboard".into())
        .spawn(move || {
            let mut cb = None;
            // What each client is known to hold, so we don't resend large text.
            let mut sent: HashMap<String, u64> = HashMap::new();
            for job in rx {
                match job {
                    Job::PushTo(client) => {
                        if let Some(text) = read_text(&mut cb) {
                            let d = digest(&text);
                            if sent.get(&client.name) != Some(&d) {
                                sent.insert(client.name.clone(), d);
                                client.send(Msg::Clipboard(text));
                            }
                        }
                    }
                    Job::FromRemote { from, text } => {
                        if !crate::flags::on(&crate::flags::CLIPBOARD) {
                            continue;
                        }
                        let d = digest(&text);
                        sent.insert(from.clone(), d);
                        write_text(&mut cb, &text);
                        log::debug!("clipboard from '{from}' ({} bytes)", text.len());
                        let active = reg.active.lock().unwrap().clone();
                        if let Some(name) = active.filter(|n| *n != from) {
                            if let Some(c) = reg.get(&name) {
                                if sent.get(&name) != Some(&d) {
                                    sent.insert(name, d);
                                    c.send(Msg::Clipboard(text));
                                }
                            }
                        }
                    }
                }
            }
        })
        .unwrap();
    tx
}

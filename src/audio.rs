//! PipeWire plumbing via `pactl` subprocess calls (talking to pipewire-pulse):
//! virtual sink creation/teardown, sink snapshots, volumes, default sink.

use std::io::Read;
use std::os::fd::AsRawFd;
use std::process::{Child, Command, Stdio};

use crate::devices::{DeviceSpec, VENDOR_ID};

// The sink *name* doubles as the display name: Steam Game Mode's audio picker
// ignores node.nick/node.description for card-less sinks and shows the raw
// sink name, so the name itself must be presentable. pipewire-pulse accepts
// names with spaces and pactl addresses them fine.
pub(crate) const GAME_SINK: &str = "Arctis Game";
pub(crate) const CHAT_SINK: &str = "Arctis Chat";
const GAME_DESC: &str = "Arctis Game";
const CHAT_DESC: &str = "Arctis Chat";
/// Sink names used by versions before the Game-Mode rename; still matched
/// during cleanup so an upgrade tears down a crashed old instance's modules.
const LEGACY_SINKS: [&str; 2] = ["arctis_game", "arctis_chat"];

fn pactl(args: &[&str]) -> Result<String, String> {
    let out = Command::new("pactl")
        .args(args)
        .output()
        .map_err(|e| format!("failed to run pactl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "pactl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// One-call snapshot of the sink situation.
#[derive(Clone)]
pub(crate) struct SinkSnapshot {
    pub(crate) game: bool,
    pub(crate) chat: bool,
    pub(crate) real: Option<String>,
}

fn snapshot_sinks(spec: &DeviceSpec) -> Option<SinkSnapshot> {
    let json = pactl(&["-f", "json", "list", "sinks"]).ok()?;
    let sinks: serde_json::Value = serde_json::from_str(&json).ok()?;
    let mut snap = SinkSnapshot {
        game: false,
        chat: false,
        real: None,
    };
    for sink in sinks.as_array()? {
        let name = sink["name"].as_str().unwrap_or("");
        match name {
            GAME_SINK => snap.game = true,
            CHAT_SINK => snap.chat = true,
            _ => {}
        }
        let props = &sink["properties"];
        let vendor = props["device.vendor.id"].as_str().unwrap_or("");
        let product = props["device.product.id"].as_str().unwrap_or("");
        if vendor == format!("{:#06x}", VENDOR_ID)
            && spec
                .product_ids
                .iter()
                .any(|p| product == format!("{:#06x}", p))
        {
            snap.real = Some(name.to_string());
        }
    }
    Some(snap)
}

fn get_default_sink() -> Option<String> {
    pactl(&["get-default-sink"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Parse one line of `pactl list modules short`: index \t name \t argument.
/// Returns (index, argument). The JSON format can't be used here: pipewire-pulse
/// omits the module index from it.
fn parse_module_line(line: &str) -> Option<(&str, &str)> {
    let mut parts = line.split('\t');
    let index = parts.next()?.trim();
    let _name = parts.next()?;
    let argument = parts.next().unwrap_or("");
    if index.is_empty() || !index.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((index, argument))
}

/// Split a module argument string into `key=value` pairs, honouring double
/// quotes and backslash escapes (pactl shows the argument text the module was
/// loaded with, e.g. `sink_name="Arctis Game" sink_properties=...`).
fn module_args(arg: &str) -> Vec<(String, String)> {
    let mut tokens: Vec<String> = Vec::new();
    let mut token = String::new();
    let mut in_quote = false;
    let mut escaped = false;
    for ch in arg.chars() {
        if escaped {
            token.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            in_quote = !in_quote;
        } else if ch.is_whitespace() && !in_quote {
            if !token.is_empty() {
                tokens.push(std::mem::take(&mut token));
            }
        } else {
            token.push(ch);
        }
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    tokens
        .iter()
        .filter_map(|t| t.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// A module is ours if we created its sink (`sink_name=`) or it loops one of
/// our sinks' monitors (`source=`). Deliberately not a substring match: a
/// user's own module routing *into* our sinks, or merely mentioning them in
/// a description, must survive cleanup.
fn module_is_ours(arg: &str) -> bool {
    let ours =
        |name: &str| name == GAME_SINK || name == CHAT_SINK || LEGACY_SINKS.contains(&name);
    module_args(arg).iter().any(|(key, value)| match key.as_str() {
        "sink_name" => ours(value),
        "source" => value.strip_suffix(".monitor").is_some_and(ours),
        _ => false,
    })
}

/// Unload any of our modules, including ones left behind by a previous
/// (crashed) instance.
fn unload_our_modules() {
    let Ok(out) = pactl(&["list", "modules", "short"]) else {
        return;
    };
    for line in out.lines() {
        let Some((index, arg)) = parse_module_line(line) else {
            continue;
        };
        if module_is_ours(arg) {
            let _ = pactl(&["unload-module", index]);
        }
    }
}

fn load_module(args: &[&str]) -> Result<u32, String> {
    let mut full = vec!["load-module"];
    full.extend_from_slice(args);
    let out = pactl(&full)?;
    out.trim()
        .parse()
        .map_err(|_| format!("unexpected pactl load-module output: {out}"))
}

/// Create the two null sinks and their loopbacks into the real sink.
/// Any pre-existing instances of our modules are cleared first.
fn setup_sinks(real_sink: &str) -> Result<(), String> {
    unload_our_modules();
    for (name, desc) in [(GAME_SINK, GAME_DESC), (CHAT_SINK, CHAT_DESC)] {
        let escaped = desc.replace(' ', "\\ ");
        load_module(&[
            "module-null-sink",
            // Names contain spaces, so they must be quoted inside the
            // module argument string. Desktop tools display
            // node.description; Steam Game Mode shows the raw sink name.
            &format!("sink_name=\"{name}\""),
            &format!(
                "sink_properties=node.description=\"{escaped}\" node.nick=\"{escaped}\""
            ),
        ])?;
        load_module(&[
            "module-loopback",
            &format!("source=\"{name}.monitor\""),
            &format!("sink={real_sink}"),
            "latency_msec=0",
        ])?;
    }
    Ok(())
}

/// The audio operations `Session` needs, behind a trait so the state machine
/// can be unit-tested without a running PipeWire (see `session::tests`).
pub(crate) trait Audio {
    fn snapshot(&self, spec: &DeviceSpec) -> Option<SinkSnapshot>;
    /// Create the virtual sinks routed into `real_sink`.
    fn setup_sinks(&self, real_sink: &str) -> Result<(), String>;
    /// Unload our modules (including a crashed instance's leftovers).
    fn teardown_sinks(&self);
    fn set_mix(&self, game: u8, chat: u8);
    fn default_sink(&self) -> Option<String>;
    fn set_default_sink(&self, sink: &str) -> Result<(), String>;
    fn notify_low_battery(&self, pct: u32);
}

/// The real implementation: pactl subprocess calls plus notify-send.
pub(crate) struct Pactl;

impl Audio for Pactl {
    fn snapshot(&self, spec: &DeviceSpec) -> Option<SinkSnapshot> {
        snapshot_sinks(spec)
    }

    fn setup_sinks(&self, real_sink: &str) -> Result<(), String> {
        setup_sinks(real_sink)
    }

    fn teardown_sinks(&self) {
        unload_our_modules();
    }

    fn set_mix(&self, game: u8, chat: u8) {
        let _ = pactl(&["set-sink-volume", GAME_SINK, &format!("{}%", game.min(100))]);
        let _ = pactl(&["set-sink-volume", CHAT_SINK, &format!("{}%", chat.min(100))]);
    }

    fn default_sink(&self) -> Option<String> {
        get_default_sink()
    }

    fn set_default_sink(&self, sink: &str) -> Result<(), String> {
        pactl(&["set-default-sink", sink]).map(|_| ())
    }

    fn notify_low_battery(&self, pct: u32) {
        let _ = Command::new("notify-send")
            .args([
                "-u",
                "critical",
                "-a",
                "rust-arctis-chatmix",
                "Arctis headset battery low",
                &format!("{pct}% remaining"),
            ])
            .status();
    }
}

/// A running `pactl subscribe`, so sink/module changes are noticed instantly
/// instead of on the next polling tick. Its stdout fd slots into the
/// session's poll loop.
pub(crate) struct SinkWatch {
    child: Child,
    buf: Vec<u8>,
}

pub(crate) enum WatchEvent {
    /// A sink or module appeared or vanished; re-check sink health.
    Changed,
    /// Only events we don't care about (client/stream noise, volume changes —
    /// including the ones our own pactl calls generate).
    Quiet,
    /// The stream ended (pipewire-pulse went away); drop the watch and poll.
    Ended,
}

impl SinkWatch {
    pub(crate) fn spawn() -> Option<Self> {
        let child = Command::new("pactl")
            .arg("subscribe")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        // Non-blocking, so drain() can safely read until the pipe is empty.
        let fd = child.stdout.as_ref()?.as_raw_fd();
        unsafe {
            libc::fcntl(fd, libc::F_SETFL, libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK);
        }
        Some(Self { child, buf: Vec::new() })
    }

    pub(crate) fn fd(&self) -> i32 {
        self.child.stdout.as_ref().expect("stdout is piped").as_raw_fd()
    }

    /// Consume whatever is pending on the event stream. Lines look like
    /// `Event 'new' on sink #57`; only new/remove on sinks and modules count
    /// as a change ('change' events fire on every volume tweak).
    pub(crate) fn drain(&mut self) -> WatchEvent {
        let stdout = self.child.stdout.as_mut().expect("stdout is piped");
        let mut ended = false;
        let mut chunk = [0u8; 512];
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) => {
                    ended = true;
                    break;
                }
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    ended = true;
                    break;
                }
            }
        }

        let mut changed = false;
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&line);
            if (line.contains("Event 'new'") || line.contains("Event 'remove'"))
                && (line.contains("on sink #") || line.contains("on module #"))
            {
                changed = true;
            }
        }

        if changed {
            WatchEvent::Changed
        } else if ended {
            WatchEvent::Ended
        } else {
            WatchEvent::Quiet
        }
    }
}

impl Drop for SinkWatch {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_line_parses() {
        let line = "536870917\tmodule-null-sink\tsink_name=arctis_chat sink_properties=node.description=\"Arctis\\ Chat\"\t";
        let (index, arg) = parse_module_line(line).unwrap();
        assert_eq!(index, "536870917");
        assert!(arg.contains("arctis_chat"));
        assert_eq!(parse_module_line(""), None);
        assert_eq!(parse_module_line("not-a-number\tname\targ"), None);
        // Modules can legitimately have no argument column.
        assert_eq!(parse_module_line("12\tmodule-x"), Some(("12", "")));
    }

    #[test]
    fn our_modules_are_recognised() {
        // Current naming (quoted, spaces) — the null sink and its loopback.
        assert!(module_is_ours(
            r#"sink_name="Arctis Game" sink_properties=node.description="Arctis\ Game" node.nick="Arctis\ Game""#
        ));
        assert!(module_is_ours(
            r#"source="Arctis Chat.monitor" sink=alsa_output.usb-SteelSeries.analog-stereo latency_msec=0"#
        ));
        // Legacy (pre-rename, unquoted) names still match for upgrade cleanup.
        assert!(module_is_ours("sink_name=arctis_chat sink_properties=x"));
        assert!(module_is_ours("source=arctis_game.monitor sink=foo"));
    }

    #[test]
    fn user_modules_mentioning_our_sinks_are_spared() {
        // A user loopback routing INTO our sink is not ours.
        assert!(!module_is_ours(r#"source=mic.monitor sink="Arctis Game""#));
        // A sink merely mentioning the name in its description is not ours.
        assert!(!module_is_ours(
            r#"sink_name=mysink sink_properties=node.description="Arctis Game copy""#
        ));
        assert!(!module_is_ours(""));
    }
}

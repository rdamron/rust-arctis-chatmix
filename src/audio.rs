//! PipeWire plumbing via `pactl` subprocess calls (talking to pipewire-pulse):
//! virtual sink creation/teardown, sink snapshots, volumes, default sink.

use std::process::Command;

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

pub(crate) fn pactl(args: &[&str]) -> Result<String, String> {
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
pub(crate) struct SinkSnapshot {
    pub(crate) game: bool,
    pub(crate) chat: bool,
    pub(crate) real: Option<String>,
}

pub(crate) fn snapshot_sinks(spec: &DeviceSpec) -> Option<SinkSnapshot> {
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

pub(crate) fn get_default_sink() -> Option<String> {
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

/// Unload any of our modules, including ones left behind by a previous
/// (crashed) instance.
pub(crate) fn unload_our_modules() {
    let Ok(out) = pactl(&["list", "modules", "short"]) else {
        return;
    };
    for line in out.lines() {
        let Some((index, arg)) = parse_module_line(line) else {
            continue;
        };
        if arg.contains(GAME_SINK)
            || arg.contains(CHAT_SINK)
            || LEGACY_SINKS.iter().any(|s| arg.contains(s))
        {
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

pub(crate) struct Sinks {
    pub(crate) real_sink: String,
}

impl Sinks {
    /// Create the two null sinks and their loopbacks into the real sink.
    /// Any pre-existing instances of our modules are cleared first.
    pub(crate) fn setup(real_sink: &str) -> Result<Self, String> {
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
        Ok(Self {
            real_sink: real_sink.to_string(),
        })
    }
}

pub(crate) fn set_mix(game: u8, chat: u8) {
    let _ = pactl(&["set-sink-volume", GAME_SINK, &format!("{}%", game.min(100))]);
    let _ = pactl(&["set-sink-volume", CHAT_SINK, &format!("{}%", chat.min(100))]);
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
}

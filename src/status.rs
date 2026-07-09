//! The one-shot `status` subcommand: query the device and print a labelled
//! report of everything it answers with.

use std::fs::OpenOptions;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use crate::devices::{parse_report, Collected, DeviceSpec, Field, REPORT_LEN};
use crate::hid::{find_hidraw, poll_ready, write_command};

/// Ordered, labelled fields for `status` output. Numeric fields (battery,
/// chatmix) are rendered specially; everything else goes through the spec's
/// describe fn.
const STATUS_LINES: &[(Field, &str)] = &[
    (Field::Power, "headset"),
    (Field::Battery, "battery"),
    (Field::SlotBattery, "charge slot"),
    (Field::MicMuted, "microphone"),
    (Field::GameMix, "chatmix"),
    (Field::Anc, "anc"),
    (Field::WirelessMode, "wireless"),
    (Field::BtConnection, "bluetooth"),
    (Field::BtAutoMute, "bt auto-mute"),
    (Field::AutoOff, "auto off"),
    (Field::Gain, "gain"),
    (Field::MicVolume, "mic volume"),
    (Field::SideTone, "sidetone"),
    (Field::LineOut, "line out"),
];

fn print_status(spec: &DeviceSpec, fields: &Collected) {
    println!("{:<14}{}", "device:", spec.name);
    for &(field, label) in STATUS_LINES {
        let Some(raw) = fields.get(field) else {
            continue;
        };
        let value = match field {
            Field::Battery => format!(
                "{}%{}",
                spec.battery_percent(raw),
                if spec.is_charging(fields) { " (charging)" } else { "" }
            ),
            Field::SlotBattery => format!("{}%", spec.battery_percent(raw)),
            Field::MicMuted => (if raw == 0x01 { "muted" } else { "unmuted" }).to_string(),
            Field::GameMix => match fields.get(Field::ChatMix) {
                Some(chat) => format!("game {}% / chat {}%", raw.min(100), chat.min(100)),
                None => continue,
            },
            _ => match (spec.describe)(fields, field) {
                Some(s) => s,
                None => continue,
            },
        };
        println!("{:<14}{}", format!("{label}:"), value);
    }
}

pub(crate) fn cmd_status() -> i32 {
    let Some(found) = find_hidraw() else {
        eprintln!("rust-arctis-chatmix: no supported Arctis device found (is it plugged in?)");
        return 1;
    };
    let spec = found.spec;
    let mut dev = match OpenOptions::new().read(true).write(true).open(&found.command) {
        Ok(d) => d,
        Err(e) => {
            eprintln!(
                "rust-arctis-chatmix: cannot open {}: {e}",
                found.command.display()
            );
            return 1;
        }
    };
    if !spec.has_status() {
        eprintln!("rust-arctis-chatmix: the {} has no status protocol", spec.name);
        return 1;
    }
    let mut unnumbered = spec.unnumbered_reports;
    for req in spec.status_requests {
        if write_command(&mut dev, req, &mut unnumbered).is_err() {
            eprintln!("rust-arctis-chatmix: failed to send status request");
            return 1;
        }
    }

    // Some devices answer with several frames (the Elite spreads status over
    // half a dozen); collect until the device goes quiet or we time out.
    let fd = dev.as_raw_fd();
    let mut fields = Collected::default();
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut got_any = false;
    while Instant::now() < deadline {
        let wait_ms = if got_any { 300 } else { 200 };
        if !poll_ready(fd, wait_ms) {
            if got_any {
                break; // device went quiet, we have our answer
            }
            continue;
        }
        let mut report = [0u8; REPORT_LEN + 1];
        let Ok(n) = dev.read(&mut report) else { break };
        let parsed = parse_report(spec, &report[..n]);
        if !parsed.is_empty() {
            fields.merge(&parsed);
            got_any = true;
        }
    }
    if !got_any {
        eprintln!("rust-arctis-chatmix: no status response from the device");
        return 1;
    }
    print_status(spec, &fields);
    0
}

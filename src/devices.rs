//! Per-device protocol specs for every headset family supported by
//! Linux-Arctis-Manager (the protocol reference for all of this):
//! https://github.com/elegos/Linux-Arctis-Manager, src/linux_arctis_manager/devices/*.yaml
//!
//! Each spec captures where the ChatMix values live in the device's HID
//! reports, the init commands needed to make the dial emit them, and how to
//! read status (battery/power/mic/...) fields. Byte offsets are relative to
//! the buffer returned by reading the hidraw node: for interfaces that use
//! numbered HID reports (Nova Pro family, Nova Elite) that buffer starts with
//! the report id; for unnumbered ones (Nova 7 / 7+ / 5) it is bare payload.
//!
//! Verified against real hardware: Nova Pro Wireless (1038:12e0) and Nova 7X
//! Gen 2 (1038:229e — which revealed that Gen 2 hardware streams dial events
//! on USB interface 5 while status stays on 3, unlike the upstream YAML).
//! The other specs are faithful translations of the upstream YAMLs
//! (cross-checked against HeadsetControl where possible) and await field
//! testing.

pub const REPORT_LEN: usize = 64;

/// A value we know how to extract from some report.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field {
    Power,
    Battery,
    SlotBattery,
    Charging,
    MicMuted,
    GameMix,
    ChatMix,
    Anc,
    Transparency,
    WirelessMode,
    Pairing,
    BtPower,
    BtConnection,
    BtAutoMute,
    AutoOff,
    Gain,
    MicVolume,
    SideTone,
    LineOut,
}

/// One report the device can send: match by prefix, extract fields at fixed
/// byte offsets (from the upstream `status.response_mapping` sections).
pub struct Frame {
    pub prefix: &'static [u8],
    pub fields: &'static [(Field, usize)],
}

/// Values extracted from one or more reports. Small enough that a vec beats
/// a map.
#[derive(Default, Debug)]
pub struct Collected(Vec<(Field, u8)>);

impl Collected {
    pub fn get(&self, field: Field) -> Option<u8> {
        self.0.iter().find(|(f, _)| *f == field).map(|(_, v)| *v)
    }

    pub fn insert(&mut self, field: Field, value: u8) {
        if let Some(slot) = self.0.iter_mut().find(|(f, _)| *f == field) {
            slot.1 = value;
        } else {
            self.0.push((field, value));
        }
    }

    pub fn merge(&mut self, other: &Collected) {
        for (f, v) in &other.0 {
            self.insert(*f, *v);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

pub struct DeviceSpec {
    pub name: &'static str,
    pub product_ids: &'static [u16],
    /// USB interface number commands/status requests are written to
    /// (HID_PHYS input<N>). Reports are read from here too.
    pub command_interface: u32,
    /// Additional interfaces to read reports from. The Nova 7 Gen 2 streams
    /// its dial events on interface 5 while status stays on 3 (verified on
    /// real hardware, 1038:229e); upstream lists the same split for the
    /// Nova 5. Interfaces that don't exist on a given model are skipped.
    pub extra_listen_interfaces: &'static [u32],
    /// Interface uses unnumbered HID reports: hidraw writes need a 0x00
    /// report-id byte prepended (the kernel strips it). Reads come back as
    /// bare payload either way, which the frame prefixes already reflect.
    pub unnumbered_reports: bool,
    /// Handshake sent once per connection. Deliberately excludes the upstream
    /// init commands that would overwrite on-device settings (EQ, sidetone,
    /// auto-off, ...); we only keep queries and the Sonar/ChatMix enables.
    pub init: &'static [&'static [u8]],
    /// Written to solicit status frames; also polled periodically.
    pub status_requests: &'static [&'static [u8]],
    /// Seconds between status polls while connected.
    pub status_poll_secs: u64,
    pub frames: &'static [Frame],
    /// Raw battery value meaning 100%; 0 when the device reports no battery.
    pub battery_max: u8,
    /// Raw Power values meaning "headset on and connected".
    pub power_online: &'static [u8],
    /// Raw Power values meaning "charging" (headset off, cable in).
    pub power_charging: &'static [u8],
    /// Raw Charging-field values meaning "charging".
    pub charging_on: &'static [u8],
    /// Human string for an extractable field, for `status` output. May read
    /// sibling fields (e.g. bluetooth power + connection make one line).
    pub describe: fn(&Collected, Field) -> Option<String>,
}

impl DeviceSpec {
    pub fn is_online(&self, power: u8) -> bool {
        self.power_online.contains(&power)
    }

    pub fn battery_percent(&self, raw: u8) -> u32 {
        if self.battery_max == 0 {
            return 0;
        }
        raw.min(self.battery_max) as u32 * 100 / self.battery_max as u32
    }

    pub fn is_charging(&self, fields: &Collected) -> bool {
        if let Some(c) = fields.get(Field::Charging) {
            return self.charging_on.contains(&c);
        }
        fields
            .get(Field::Power)
            .is_some_and(|p| self.power_charging.contains(&p))
    }

    pub fn has_chatmix(&self) -> bool {
        self.frames
            .iter()
            .any(|f| f.fields.iter().any(|(field, _)| *field == Field::GameMix))
    }

    pub fn has_status(&self) -> bool {
        !self.status_requests.is_empty()
    }
}

/// Frame a command for a hidraw write: 0x00 report-id prefix when the
/// interface uses unnumbered reports, zero-padded to the report length.
pub fn frame_command(cmd: &[u8], unnumbered: bool) -> Vec<u8> {
    let mut buf = Vec::with_capacity(REPORT_LEN + 1);
    if unnumbered {
        buf.push(0x00);
    }
    buf.extend_from_slice(cmd);
    buf.resize(REPORT_LEN + usize::from(unnumbered), 0x00);
    buf
}

/// Match a report against the spec's frames and extract whatever fields fit.
pub fn parse_report(spec: &DeviceSpec, report: &[u8]) -> Collected {
    let mut out = Collected::default();
    for frame in spec.frames {
        if report.len() >= frame.prefix.len() && report.starts_with(frame.prefix) {
            for &(field, offset) in frame.fields {
                if let Some(&v) = report.get(offset) {
                    out.insert(field, v);
                }
            }
            break;
        }
    }
    out
}

fn map(v: u8, pairs: &[(u8, &str)]) -> String {
    pairs
        .iter()
        .find(|(k, _)| *k == v)
        .map(|(_, s)| s.to_string())
        .unwrap_or_else(|| format!("unknown ({v:#04x})"))
}

// ---------------------------------------------------------------------------
// Arctis Nova Pro Wireless — verified on real hardware.
// ---------------------------------------------------------------------------

fn describe_nova_pro_wireless(fields: &Collected, field: Field) -> Option<String> {
    let v = fields.get(field)?;
    Some(match field {
        Field::Power => map(v, &[(0x01, "offline"), (0x02, "charging via cable"), (0x08, "online")]),
        Field::Anc => {
            let base = map(v, &[(0x00, "off"), (0x01, "transparent"), (0x02, "on")]);
            match fields.get(Field::Transparency) {
                Some(t) => format!("{base} (transparency level {t}/10)"),
                None => base,
            }
        }
        Field::WirelessMode => {
            let mode = map(v, &[(0x00, "speed"), (0x01, "range")]);
            let pairing = fields.get(Field::Pairing).map(|p| {
                map(p, &[(0x01, "not paired"), (0x04, "paired (offline)"), (0x08, "connected")])
            });
            match pairing {
                Some(p) => format!("{mode} mode, {p}"),
                None => format!("{mode} mode"),
            }
        }
        Field::BtConnection => {
            if fields.get(Field::BtPower) != Some(0x00) {
                "off".to_string()
            } else {
                map(v, &[(0x01, "on, connected"), (0x02, "on, disconnected")])
            }
        }
        Field::AutoOff => map(
            v,
            &[
                (0x00, "never"),
                (0x01, "1 minute"),
                (0x02, "5 minutes"),
                (0x03, "10 minutes"),
                (0x04, "15 minutes"),
                (0x05, "30 minutes"),
                (0x06, "60 minutes"),
            ],
        ),
        _ => return None,
    })
}

pub static NOVA_PRO_WIRELESS: DeviceSpec = DeviceSpec {
    name: "Arctis Nova Pro Wireless",
    product_ids: &[0x12e0, 0x12e5, 0x225d],
    command_interface: 4,
    extra_listen_interfaces: &[],
    unnumbered_reports: false,
    // From nova_pro_wireless.yaml `device_init`. 0x8d 0x01 enables Sonar mode
    // and 0x49 0x01 enables ChatMix — without them the wheel button won't
    // switch the base station into game/chat mixing.
    init: &[
        &[0x06, 0x20],
        &[0x06, 0x10],
        &[0x06, 0x3b],
        &[0x06, 0x8d, 0x01],
        &[0x06, 0x80],
        &[0x06, 0x85, 0x0a],
        &[0x06, 0xb2],
        &[0x06, 0x47, 0x64, 0x00, 0x64],
        &[0x06, 0x83, 0x01],
        &[0x06, 0x89, 0x00],
        &[0x06, 0xb3, 0x00],
        &[0x06, 0x43, 0x01],
        &[0x06, 0x69, 0x00],
        &[0x06, 0x3b, 0x00],
        &[0x06, 0x8d, 0x01],
        &[0x06, 0x49, 0x01],
        &[0x06, 0xb7, 0x00],
    ],
    status_requests: &[&[0x06, 0xb0]],
    status_poll_secs: 10,
    frames: &[
        Frame {
            prefix: &[0x07, 0x45],
            fields: &[(Field::GameMix, 2), (Field::ChatMix, 3)],
        },
        Frame {
            prefix: &[0x06, 0xb0],
            fields: &[
                (Field::BtPower, 4),
                (Field::BtConnection, 5),
                (Field::Battery, 6),
                (Field::SlotBattery, 7),
                (Field::Transparency, 8),
                (Field::MicMuted, 9),
                (Field::Anc, 10),
                (Field::AutoOff, 12),
                (Field::WirelessMode, 13),
                (Field::Pairing, 14),
                (Field::Power, 15),
            ],
        },
    ],
    battery_max: 8,
    power_online: &[0x08],
    power_charging: &[0x02],
    charging_on: &[],
    describe: describe_nova_pro_wireless,
};

// ---------------------------------------------------------------------------
// Arctis Nova Pro (wired GameDAC Gen 2) — untested.
// ---------------------------------------------------------------------------

fn describe_nova_pro_wired(fields: &Collected, field: Field) -> Option<String> {
    let v = fields.get(field)?;
    Some(match field {
        Field::Gain => map(v, &[(0x01, "low"), (0x02, "high")]),
        Field::SideTone => map(v, &[(0x00, "off"), (0x01, "low"), (0x02, "medium"), (0x03, "high")]),
        Field::MicVolume => format!("{v}/10"),
        Field::LineOut => map(v, &[(0x01, "speaker"), (0x02, "stream")]),
        _ => return None,
    })
}

pub static NOVA_PRO_WIRED: DeviceSpec = DeviceSpec {
    name: "Arctis Nova Pro (wired GameDAC Gen 2)",
    product_ids: &[0x12cd, 0x12cb],
    command_interface: 4,
    extra_listen_interfaces: &[],
    unnumbered_reports: false,
    // nova_pro_wired.yaml `device_init` minus settings writes; 0x49/0x8d are
    // the same ChatMix/Sonar enables as the wireless base station.
    init: &[
        &[0x06, 0x10],
        &[0x06, 0x20],
        &[0x06, 0x80],
        &[0x06, 0x18],
        &[0x06, 0x49, 0x01],
        &[0x06, 0x8d, 0x01],
    ],
    // Response 0x06 0x20 carries the current settings snapshot.
    status_requests: &[&[0x06, 0x20]],
    status_poll_secs: 10,
    frames: &[
        Frame {
            prefix: &[0x07, 0x45],
            fields: &[(Field::GameMix, 2), (Field::ChatMix, 3)],
        },
        Frame {
            prefix: &[0x06, 0x20],
            fields: &[
                (Field::Gain, 4),
                (Field::MicVolume, 0x10),
                (Field::SideTone, 0x11),
                (Field::LineOut, 0x12),
            ],
        },
    ],
    battery_max: 0, // wired, no battery
    power_online: &[],
    power_charging: &[],
    charging_on: &[],
    describe: describe_nova_pro_wired,
};

// ---------------------------------------------------------------------------
// Arctis Nova 7 family. Two entries because older firmware reports battery
// as 0-4 while newer reports 0-100 (upstream claims 0-128; hardware says
// otherwise — see NOVA_7_PERCENT). Only the 7X Gen 2 (0x229e) is verified
// on real hardware.
// ---------------------------------------------------------------------------

fn describe_nova_7(fields: &Collected, field: Field) -> Option<String> {
    let v = fields.get(field)?;
    Some(match field {
        Field::Power => map(v, &[(0x02, "offline"), (0x03, "online")]),
        Field::BtConnection => {
            map(v, &[(0x00, "off"), (0x02, "connected"), (0x03, "disconnected")])
        }
        Field::BtAutoMute => map(v, &[(0x00, "off"), (0x01, "-12db"), (0x02, "on")]),
        _ => return None,
    })
}

const NOVA_7_INIT: &[&[u8]] = &[&[0x10], &[0x09]];

const NOVA_7_FRAMES: &[Frame] = &[
    // Unsolicited dial report.
    Frame {
        prefix: &[0x45],
        fields: &[(Field::GameMix, 1), (Field::ChatMix, 2)],
    },
    Frame {
        prefix: &[0xb0],
        fields: &[
            (Field::Power, 1),
            (Field::Battery, 2),
            (Field::Charging, 3),
            (Field::GameMix, 4),
            (Field::ChatMix, 5),
            (Field::BtConnection, 6),
            (Field::BtPower, 7),
            (Field::BtAutoMute, 8),
            (Field::MicMuted, 9),
        ],
    },
];

pub static NOVA_7_DISCRETE: DeviceSpec = DeviceSpec {
    name: "Arctis Nova 7 (discrete battery)",
    product_ids: &[0x2202, 0x2206, 0x22a4, 0x223a, 0x227a, 0x22ab],
    command_interface: 3,
    extra_listen_interfaces: &[5],
    unnumbered_reports: true,
    init: NOVA_7_INIT,
    status_requests: &[&[0xb0]],
    status_poll_secs: 10,
    frames: NOVA_7_FRAMES,
    battery_max: 4,
    power_online: &[0x03],
    power_charging: &[],
    charging_on: &[0x01],
    describe: describe_nova_7,
};

pub static NOVA_7_PERCENT: DeviceSpec = DeviceSpec {
    name: "Arctis Nova 7 (percent battery)",
    product_ids: &[0x22a1, 0x227e, 0x2258, 0x229e, 0x22a9, 0x22a5],
    command_interface: 3,
    extra_listen_interfaces: &[5],
    unnumbered_reports: true,
    init: NOVA_7_INIT,
    status_requests: &[&[0xb0]],
    status_poll_secs: 10,
    frames: NOVA_7_FRAMES,
    // Upstream says 0-128, but a Nova 7X Gen 2 (0x229e) at full charge
    // reports 0x64 = 100 on real hardware: it's a plain percent.
    battery_max: 100,
    power_online: &[0x03],
    power_charging: &[],
    charging_on: &[0x01],
    describe: describe_nova_7,
};

// ---------------------------------------------------------------------------
// Arctis 7+ — untested.
// ---------------------------------------------------------------------------

fn describe_arctis_7_plus(fields: &Collected, field: Field) -> Option<String> {
    let v = fields.get(field)?;
    Some(match field {
        Field::Power => map(v, &[(0x00, "online"), (0x01, "offline"), (0x02, "online"), (0x03, "online")]),
        _ => return None,
    })
}

pub static ARCTIS_7_PLUS: DeviceSpec = DeviceSpec {
    name: "Arctis 7+",
    product_ids: &[0x220e, 0x2212, 0x2216, 0x2236],
    command_interface: 3,
    extra_listen_interfaces: &[5],
    unnumbered_reports: true,
    init: &[],
    status_requests: &[&[0xb0]],
    // Upstream only documents ChatMix inside the 0xb0 status frame; poll
    // fast enough that the dial feels responsive even if the 0x45 dial
    // report below (borrowed from the Nova 7, same generation) never comes.
    status_poll_secs: 2,
    frames: &[
        Frame {
            prefix: &[0x45],
            fields: &[(Field::GameMix, 1), (Field::ChatMix, 2)],
        },
        Frame {
            prefix: &[0xb0],
            fields: &[
                (Field::Power, 1),
                (Field::Battery, 2),
                (Field::Charging, 3),
                (Field::GameMix, 4),
                (Field::ChatMix, 5),
            ],
        },
    ],
    battery_max: 4,
    power_online: &[0x00, 0x02, 0x03],
    power_charging: &[],
    charging_on: &[0x01],
    describe: describe_arctis_7_plus,
};

// ---------------------------------------------------------------------------
// Arctis Nova 5 / 5X — untested. Upstream documents no ChatMix reports for
// this family, so we only provide the virtual sinks (manual volume control)
// and battery/status.
// ---------------------------------------------------------------------------

fn describe_nova_5(fields: &Collected, field: Field) -> Option<String> {
    let v = fields.get(field)?;
    Some(match field {
        Field::Power => map(v, &[(0x02, "offline"), (0x03, "online")]),
        Field::WirelessMode => map(v, &[(0x00, "range"), (0x01, "speed")]),
        _ => return None,
    })
}

pub static NOVA_5: DeviceSpec = DeviceSpec {
    name: "Arctis Nova 5",
    product_ids: &[0x2232, 0x2253, 0x2264],
    command_interface: 3,
    extra_listen_interfaces: &[5],
    unnumbered_reports: true,
    init: &[&[0x10], &[0x36], &[0x09]],
    status_requests: &[&[0xb0], &[0x36]],
    status_poll_secs: 10,
    frames: &[
        Frame {
            prefix: &[0xb0],
            fields: &[(Field::Power, 1), (Field::Battery, 2), (Field::Charging, 3)],
        },
        Frame {
            prefix: &[0x36],
            fields: &[(Field::WirelessMode, 1)],
        },
    ],
    battery_max: 100,
    power_online: &[0x03],
    power_charging: &[],
    charging_on: &[0x01],
    describe: describe_nova_5,
};

// ---------------------------------------------------------------------------
// Arctis Nova Elite — untested.
// ---------------------------------------------------------------------------

fn describe_nova_elite(fields: &Collected, field: Field) -> Option<String> {
    let v = fields.get(field)?;
    Some(match field {
        Field::Power => map(
            v,
            &[(0x01, "offline"), (0x02, "charging via cable"), (0x04, "standby"), (0x08, "online")],
        ),
        Field::Anc => map(v, &[(0x00, "off"), (0x01, "transparent"), (0x02, "on")]),
        Field::WirelessMode => map(v, &[(0x00, "speed"), (0x01, "range")]),
        Field::BtConnection => map(
            v,
            &[(0x00, "off"), (0x01, "connected"), (0x02, "disconnected"), (0x04, "active")],
        ),
        Field::AutoOff => map(
            v,
            &[
                (0x00, "never"),
                (0x01, "1 minute"),
                (0x02, "5 minutes"),
                (0x03, "10 minutes"),
                (0x04, "15 minutes"),
                (0x05, "30 minutes"),
                (0x06, "60 minutes"),
            ],
        ),
        _ => return None,
    })
}

pub static NOVA_ELITE: DeviceSpec = DeviceSpec {
    name: "Arctis Nova Elite",
    product_ids: &[0x2244],
    command_interface: 3,
    extra_listen_interfaces: &[],
    unnumbered_reports: false, // commands are report id 0x01, responses 0x07
    // Same 0x8d/0x49 Sonar/ChatMix enables as the Nova Pro, report id 0x01.
    // Upstream notes the station forgets these on power cycle, which our
    // reconnect logic re-sends anyway.
    init: &[&[0x01, 0x10], &[0x01, 0x8d, 0x01], &[0x01, 0x49, 0x01]],
    status_requests: &[&[0x01, 0xb0]],
    status_poll_secs: 10,
    frames: &[
        Frame {
            prefix: &[0x07, 0x45],
            fields: &[(Field::GameMix, 2), (Field::ChatMix, 3)],
        },
        Frame {
            prefix: &[0x07, 0xb5],
            fields: &[(Field::BtConnection, 3), (Field::Power, 4)],
        },
        Frame {
            prefix: &[0x07, 0xb7],
            fields: &[(Field::Battery, 2), (Field::SlotBattery, 3), (Field::Charging, 4)],
        },
        Frame {
            prefix: &[0x07, 0xbb],
            fields: &[(Field::MicMuted, 2)],
        },
        Frame {
            prefix: &[0x07, 0xbd],
            fields: &[(Field::Anc, 2)],
        },
        Frame {
            prefix: &[0x07, 0xc1],
            fields: &[(Field::AutoOff, 2)],
        },
        Frame {
            prefix: &[0x07, 0xc3],
            fields: &[(Field::WirelessMode, 2)],
        },
    ],
    battery_max: 100,
    power_online: &[0x08],
    power_charging: &[0x02],
    charging_on: &[0x02],
    describe: describe_nova_elite,
};

pub static SPECS: &[&DeviceSpec] = &[
    &NOVA_PRO_WIRELESS,
    &NOVA_PRO_WIRED,
    &NOVA_7_DISCRETE,
    &NOVA_7_PERCENT,
    &ARCTIS_7_PLUS,
    &NOVA_5,
    &NOVA_ELITE,
];

pub const VENDOR_ID: u16 = 0x1038;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_duplicate_product_ids() {
        let mut all: Vec<u16> = SPECS.iter().flat_map(|s| s.product_ids.iter().copied()).collect();
        let n = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(n, all.len());
    }

    #[test]
    fn udev_rules_cover_all_product_ids() {
        let rules = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/packaging/70-rust-arctis-chatmix.rules"
        ));
        let mut in_rules: Vec<u16> = rules
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
            .map(|l| {
                let vendor = l
                    .split("ATTRS{idVendor}==\"")
                    .nth(1)
                    .and_then(|s| s.split('"').next())
                    .unwrap_or_else(|| panic!("no idVendor in rule line: {l}"));
                assert_eq!(
                    u16::from_str_radix(vendor, 16).unwrap(),
                    VENDOR_ID,
                    "wrong vendor in rule line: {l}"
                );
                let product = l
                    .split("ATTRS{idProduct}==\"")
                    .nth(1)
                    .and_then(|s| s.split('"').next())
                    .unwrap_or_else(|| panic!("no idProduct in rule line: {l}"));
                u16::from_str_radix(product, 16).unwrap()
            })
            .collect();
        let mut in_specs: Vec<u16> =
            SPECS.iter().flat_map(|s| s.product_ids.iter().copied()).collect();
        in_rules.sort_unstable();
        in_specs.sort_unstable();
        assert_eq!(
            in_rules, in_specs,
            "packaging/70-rust-arctis-chatmix.rules is out of sync with SPECS"
        );
    }

    #[test]
    fn frame_command_numbered() {
        let framed = frame_command(&[0x06, 0xb0], false);
        assert_eq!(framed.len(), REPORT_LEN);
        assert_eq!(&framed[..2], &[0x06, 0xb0]);
        assert!(framed[2..].iter().all(|&b| b == 0));
    }

    #[test]
    fn frame_command_unnumbered_prepends_zero_report_id() {
        let framed = frame_command(&[0xb0], true);
        assert_eq!(framed.len(), REPORT_LEN + 1);
        assert_eq!(&framed[..2], &[0x00, 0xb0]);
        assert!(framed[2..].iter().all(|&b| b == 0));
    }

    #[test]
    fn all_commands_fit_report() {
        for spec in SPECS {
            for cmd in spec.init.iter().chain(spec.status_requests) {
                assert!(cmd.len() <= REPORT_LEN, "{}: command too long", spec.name);
            }
        }
    }

    // Captured live: headset online, battery 7/8, ANC off, mic unmuted,
    // 30 min auto-off, range mode, paired+connected.
    const NPW_STATUS: [u8; 16] = [
        0x06, 0xb0, 0x00, 0x00, 0x01, 0x00, 0x07, 0x08, 0x01, 0x00, 0x00, 0x0a, 0x05, 0x01, 0x08,
        0x08,
    ];

    #[test]
    fn nova_pro_wireless_status_parses() {
        let f = parse_report(&NOVA_PRO_WIRELESS, &NPW_STATUS);
        assert_eq!(f.get(Field::Power), Some(0x08));
        assert!(NOVA_PRO_WIRELESS.is_online(0x08));
        assert!(!NOVA_PRO_WIRELESS.is_charging(&f));
        assert_eq!(NOVA_PRO_WIRELESS.battery_percent(f.get(Field::Battery).unwrap()), 87);
        assert_eq!(NOVA_PRO_WIRELESS.battery_percent(f.get(Field::SlotBattery).unwrap()), 100);
        assert_eq!(f.get(Field::MicMuted), Some(0x00));
        let d = NOVA_PRO_WIRELESS.describe;
        assert_eq!(d(&f, Field::Power).as_deref(), Some("online"));
        assert_eq!(d(&f, Field::Anc).as_deref(), Some("off (transparency level 1/10)"));
        assert_eq!(d(&f, Field::WirelessMode).as_deref(), Some("range mode, connected"));
        assert_eq!(d(&f, Field::AutoOff).as_deref(), Some("30 minutes"));
        assert_eq!(d(&f, Field::BtConnection).as_deref(), Some("off"));
    }

    #[test]
    fn nova_pro_wireless_wheel_parses() {
        let mut report = [0u8; REPORT_LEN];
        report[..4].copy_from_slice(&[0x07, 0x45, 60, 100]);
        let f = parse_report(&NOVA_PRO_WIRELESS, &report);
        assert_eq!(f.get(Field::GameMix), Some(60));
        assert_eq!(f.get(Field::ChatMix), Some(100));
    }

    #[test]
    fn nova_7_dial_and_status_parse() {
        // Synthetic, per nova_7_wireless_*.yaml response_mapping.
        let f = parse_report(&NOVA_7_PERCENT, &[0x45, 30, 80]);
        assert_eq!(f.get(Field::GameMix), Some(30));
        assert_eq!(f.get(Field::ChatMix), Some(80));

        let status = [0xb0, 0x03, 64, 0x01, 100, 100, 0x02, 0x00, 0x00, 0x01];
        let f = parse_report(&NOVA_7_PERCENT, &status);
        assert!(NOVA_7_PERCENT.is_online(f.get(Field::Power).unwrap()));
        assert_eq!(NOVA_7_PERCENT.battery_percent(64), 64);
        assert_eq!(NOVA_7_PERCENT.battery_percent(100), 100);
        assert_eq!(NOVA_7_DISCRETE.battery_percent(2), 50);
        assert!(NOVA_7_PERCENT.is_charging(&f));
        assert_eq!(f.get(Field::MicMuted), Some(0x01));
        assert_eq!(f.get(Field::GameMix), Some(100));
    }

    #[test]
    fn arctis_7_plus_status_parses() {
        // Synthetic, per arctis_7_plus.yaml: power/battery/charging/media/chat.
        let f = parse_report(&ARCTIS_7_PLUS, &[0xb0, 0x00, 3, 0x00, 40, 60]);
        assert!(ARCTIS_7_PLUS.is_online(f.get(Field::Power).unwrap()));
        assert_eq!(ARCTIS_7_PLUS.battery_percent(3), 75);
        assert!(!ARCTIS_7_PLUS.is_charging(&f));
        assert_eq!(f.get(Field::GameMix), Some(40));
        assert_eq!(f.get(Field::ChatMix), Some(60));
        assert!(!ARCTIS_7_PLUS.is_online(0x01));
    }

    #[test]
    fn nova_5_has_no_chatmix() {
        assert!(!NOVA_5.has_chatmix());
        let f = parse_report(&NOVA_5, &[0xb0, 0x03, 85, 0x03]);
        assert!(NOVA_5.is_online(0x03));
        assert_eq!(NOVA_5.battery_percent(85), 85);
        assert!(!NOVA_5.is_charging(&f));
        let f = parse_report(&NOVA_5, &[0x36, 0x01]);
        assert_eq!((NOVA_5.describe)(&f, Field::WirelessMode).as_deref(), Some("speed"));
    }

    #[test]
    fn nova_elite_multi_frame_status() {
        let mut all = Collected::default();
        all.merge(&parse_report(&NOVA_ELITE, &[0x07, 0xb5, 0x00, 0x01, 0x08]));
        all.merge(&parse_report(&NOVA_ELITE, &[0x07, 0xb7, 90, 100, 0x08]));
        all.merge(&parse_report(&NOVA_ELITE, &[0x07, 0xbb, 0x01]));
        all.merge(&parse_report(&NOVA_ELITE, &[0x07, 0x45, 25, 75]));
        assert!(NOVA_ELITE.is_online(all.get(Field::Power).unwrap()));
        assert_eq!(NOVA_ELITE.battery_percent(all.get(Field::Battery).unwrap()), 90);
        assert!(!NOVA_ELITE.is_charging(&all));
        assert_eq!(all.get(Field::MicMuted), Some(0x01));
        assert_eq!(all.get(Field::GameMix), Some(25));
        assert_eq!((NOVA_ELITE.describe)(&all, Field::BtConnection).as_deref(), Some("connected"));
    }

    #[test]
    fn unmatched_reports_yield_nothing() {
        assert!(parse_report(&NOVA_PRO_WIRELESS, &[0x01, 0x02, 0x03]).is_empty());
        assert!(parse_report(&NOVA_7_PERCENT, &[]).is_empty());
        assert!(parse_report(&NOVA_ELITE, &[0x07]).is_empty());
    }
}

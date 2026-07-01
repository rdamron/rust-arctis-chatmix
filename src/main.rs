//! ChatMix daemon for the SteelSeries Arctis Nova Pro Wireless base station.
//!
//! Creates two virtual PipeWire sinks ("Arctis Game" / "Arctis Chat") routed to the
//! headset's real sink, then listens on the base station's HID interface for ChatMix
//! wheel events and applies them as the two sinks' volumes. Self-heals when the
//! audio device disappears (headset off, profile disabled, PipeWire restart,
//! USB unplug, suspend/resume) and hands the default sink back while the
//! headset is powered off.
//!
//! Protocol reference: https://github.com/elegos/Linux-Arctis-Manager
//! (devices/nova_pro_wireless.yaml): commands are 64 bytes zero-padded, sent on USB
//! interface 4; wheel events are input reports starting 0x07 0x45 with
//! byte 2 = game mix (0-100) and byte 3 = chat mix (0-100).

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const VENDOR_ID: u32 = 0x1038;
const PRODUCT_IDS: [u32; 3] = [0x12e0, 0x12e5, 0x225d];
const HID_INTERFACE: u32 = 4;
const REPORT_LEN: usize = 64;
const STATUS_REQUEST: [u8; 2] = [0x06, 0xb0];

const GAME_SINK: &str = "arctis_game";
const CHAT_SINK: &str = "arctis_chat";
const GAME_DESC: &str = "Arctis Game";
const CHAT_DESC: &str = "Arctis Chat";

/// How often to verify the real sink and virtual sinks still exist.
const HEALTH_INTERVAL: Duration = Duration::from_secs(3);
/// How often to ask the base station for a status report (battery, power).
const STATUS_POLL_INTERVAL: Duration = Duration::from_secs(10);
/// Battery levels are 0-8; warn at <=2 (25%), clear at >=4 or when charging.
const LOW_BATTERY_WARN: u8 = 2;
const LOW_BATTERY_CLEAR: u8 = 4;

/// Init handshake from Linux-Arctis-Manager's nova_pro_wireless.yaml `device_init`,
/// minus the commands that would overwrite on-device settings (EQ 0x33/0x2e,
/// wireless mode 0xc3, auto-shutdown 0xc1, mic volume 0x37, gain 0x27,
/// sidetone 0x39, mic LED 0xbf). The important ones: 0x8d 0x01 enables Sonar
/// mode and 0x49 0x01 enables ChatMix — without them the wheel button won't
/// switch the base station into game/chat mixing.
const INIT_COMMANDS: &[&[u8]] = &[
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
];

static RUNNING: AtomicBool = AtomicBool::new(true);
static VERBOSE: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: libc::c_int) {
    RUNNING.store(false, Ordering::SeqCst);
}

fn install_signal_handlers() {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_signal as *const () as libc::sighandler_t;
        libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
        libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut());
    }
}

fn log(msg: &str) {
    println!("arctis-chatmix: {msg}");
}

fn log_verbose(msg: &str) {
    if VERBOSE.load(Ordering::Relaxed) {
        log(msg);
    }
}

// ---------------------------------------------------------------------------
// HID device discovery and report parsing
// ---------------------------------------------------------------------------

fn hid_ids() -> Vec<String> {
    PRODUCT_IDS
        .iter()
        .map(|p| format!("0003:{:08X}:{:08X}", VENDOR_ID, p))
        .collect()
}

/// Match a /sys/class/hidraw/*/device/uevent: HID_ID carries vendor/product,
/// HID_PHYS ends in "/input<usb interface number>".
fn uevent_matches(content: &str, ids: &[String], phys_suffix: &str) -> bool {
    let mut id_match = false;
    let mut phys_match = false;
    for line in content.lines() {
        if let Some(v) = line.strip_prefix("HID_ID=") {
            id_match = ids.iter().any(|id| v == id);
        } else if let Some(v) = line.strip_prefix("HID_PHYS=") {
            phys_match = v.ends_with(phys_suffix);
        }
    }
    id_match && phys_match
}

fn find_hidraw() -> Option<PathBuf> {
    let ids = hid_ids();
    let phys_suffix = format!("/input{}", HID_INTERFACE);
    for entry in fs::read_dir("/sys/class/hidraw").ok()?.flatten() {
        let uevent = entry.path().join("device/uevent");
        let Ok(content) = fs::read_to_string(&uevent) else {
            continue;
        };
        if uevent_matches(&content, &ids, &phys_suffix) {
            return Some(PathBuf::from("/dev").join(entry.file_name()));
        }
    }
    None
}

/// ChatMix wheel report: 07 45 <game 0-100> <chat 0-100>.
fn parse_wheel(report: &[u8]) -> Option<(u8, u8)> {
    if report.len() >= 4 && report[0] == 0x07 && report[1] == 0x45 {
        Some((report[2].min(100), report[3].min(100)))
    } else {
        None
    }
}

/// Parsed 06 b0 status report. Byte offsets and value meanings from
/// nova_pro_wireless.yaml `status.response_mapping` / `status_parse`.
#[derive(Debug, PartialEq)]
struct HeadsetStatus {
    bt_powerup: u8,
    bt_auto_mute: u8,
    bt_power: u8,
    bt_connection: u8,
    battery: u8,      // 0-8
    slot_battery: u8, // 0-8
    transparent_level: u8,
    mic_muted: bool,
    noise_cancelling: u8,
    auto_off: u8,
    wireless_mode: u8,
    pairing: u8,
    power: u8,
}

impl HeadsetStatus {
    fn parse(report: &[u8]) -> Option<Self> {
        if report.len() < 16 || report[0] != 0x06 || report[1] != 0xb0 {
            return None;
        }
        Some(Self {
            bt_powerup: report[2],
            bt_auto_mute: report[3],
            bt_power: report[4],
            bt_connection: report[5],
            battery: report[6].min(8),
            slot_battery: report[7].min(8),
            transparent_level: report[8],
            mic_muted: report[9] == 0x01,
            noise_cancelling: report[10],
            auto_off: report[12],
            wireless_mode: report[13],
            pairing: report[14],
            power: report[15],
        })
    }

    fn is_online(&self) -> bool {
        self.power == 0x08
    }

    fn is_charging(&self) -> bool {
        self.power == 0x02
    }

    fn battery_percent(&self) -> u32 {
        self.battery as u32 * 100 / 8
    }

    fn slot_battery_percent(&self) -> u32 {
        self.slot_battery as u32 * 100 / 8
    }

    fn power_str(&self) -> &'static str {
        match self.power {
            0x01 => "offline",
            0x02 => "charging via cable",
            0x08 => "online",
            _ => "unknown",
        }
    }

    fn anc_str(&self) -> &'static str {
        match self.noise_cancelling {
            0x00 => "off",
            0x01 => "transparent",
            0x02 => "on",
            _ => "unknown",
        }
    }

    fn wireless_mode_str(&self) -> &'static str {
        match self.wireless_mode {
            0x00 => "speed",
            0x01 => "range",
            _ => "unknown",
        }
    }

    fn pairing_str(&self) -> &'static str {
        match self.pairing {
            0x01 => "not paired",
            0x04 => "paired (offline)",
            0x08 => "connected",
            _ => "unknown",
        }
    }

    fn auto_off_str(&self) -> String {
        match self.auto_off {
            0x00 => "never".to_string(),
            0x01 => "1 minute".to_string(),
            0x02 => "5 minutes".to_string(),
            0x03 => "10 minutes".to_string(),
            0x04 => "15 minutes".to_string(),
            0x05 => "30 minutes".to_string(),
            0x06 => "60 minutes".to_string(),
            v => format!("unknown ({v:#04x})"),
        }
    }

    fn bluetooth_str(&self) -> &'static str {
        if self.bt_power != 0x00 {
            return "off";
        }
        match self.bt_connection {
            0x01 => "on, connected",
            0x02 => "on, disconnected",
            _ => "on",
        }
    }
}

fn pad_command(cmd: &[u8]) -> [u8; REPORT_LEN] {
    let mut report = [0u8; REPORT_LEN];
    report[..cmd.len()].copy_from_slice(cmd);
    report
}

/// True if the fd has data or an error condition pending (either way, the next
/// read() will not block: it returns a report or the disconnect error).
fn poll_ready(fd: i32, timeout_ms: i32) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    unsafe { libc::poll(&mut pfd, 1, timeout_ms) > 0 && pfd.revents != 0 }
}

// ---------------------------------------------------------------------------
// PipeWire (via pactl, talking to pipewire-pulse)
// ---------------------------------------------------------------------------

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
struct SinkSnapshot {
    game: bool,
    chat: bool,
    real: Option<String>,
}

fn snapshot_sinks() -> Option<SinkSnapshot> {
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
            && PRODUCT_IDS.iter().any(|p| product == format!("{:#06x}", p))
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
        if arg.contains(GAME_SINK) || arg.contains(CHAT_SINK) {
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

struct Sinks {
    real_sink: String,
}

impl Sinks {
    /// Create the two null sinks and their loopbacks into the real sink.
    /// Any pre-existing instances of our modules are cleared first.
    fn setup(real_sink: &str) -> Result<Self, String> {
        unload_our_modules();
        for (name, desc) in [(GAME_SINK, GAME_DESC), (CHAT_SINK, CHAT_DESC)] {
            let escaped = desc.replace(' ', "\\ ");
            load_module(&[
                "module-null-sink",
                &format!("sink_name={name}"),
                &format!("sink_properties=node.description=\"{escaped}\""),
            ])?;
            load_module(&[
                "module-loopback",
                &format!("source={name}.monitor"),
                &format!("sink={real_sink}"),
                "latency_msec=0",
            ])?;
        }
        Ok(Self {
            real_sink: real_sink.to_string(),
        })
    }
}

fn set_mix(game: u8, chat: u8) {
    let _ = pactl(&["set-sink-volume", GAME_SINK, &format!("{}%", game.min(100))]);
    let _ = pactl(&["set-sink-volume", CHAT_SINK, &format!("{}%", chat.min(100))]);
}

// ---------------------------------------------------------------------------
// Daemon session
// ---------------------------------------------------------------------------

struct Session {
    sinks: Option<Sinks>,
    /// Default sink before we claimed it; restored when the headset powers
    /// off and on exit.
    prev_default: Option<String>,
    manage_default: bool,
    last_mix: (u8, u8),
    /// None until the first status report.
    online: Option<bool>,
    battery_warned: bool,
    warned_no_sink: bool,
}

impl Session {
    fn new(manage_default: bool) -> Self {
        Self {
            sinks: None,
            prev_default: None,
            manage_default,
            last_mix: (100, 100),
            online: None,
            battery_warned: false,
            warned_no_sink: false,
        }
    }

    fn claim_default(&self) {
        if self.manage_default && self.sinks.is_some() {
            let _ = pactl(&["set-default-sink", GAME_SINK]);
        }
    }

    fn release_default(&self) {
        if !self.manage_default {
            return;
        }
        // Only hand it back if we still hold it — don't stomp on a choice
        // the user made in the meantime.
        let current = get_default_sink();
        if !matches!(current.as_deref(), Some(GAME_SINK) | Some(CHAT_SINK)) {
            return;
        }
        if let Some(prev) = &self.prev_default {
            if pactl(&["set-default-sink", prev]).is_ok() {
                return;
            }
        }
        if let Some(sinks) = &self.sinks {
            let _ = pactl(&["set-default-sink", &sinks.real_sink]);
        }
    }

    /// Health check: make sure the real sink exists and our virtual sinks are
    /// alive, (re)creating them as needed. Handles first-time setup, the audio
    /// device disappearing (profile off / unplug), and PipeWire restarts.
    fn ensure_sinks(&mut self) {
        let Some(snap) = snapshot_sinks() else {
            return; // pactl unavailable right now (pipewire restarting); retry next tick
        };

        let Some(real) = snap.real else {
            if self.sinks.take().is_some() {
                log("Arctis audio sink disappeared, removing virtual sinks");
                unload_our_modules();
            } else if !self.warned_no_sink {
                log("waiting for Arctis audio sink...");
                self.warned_no_sink = true;
            }
            return;
        };

        let need_setup = match &self.sinks {
            None => true,
            Some(s) => s.real_sink != real || !snap.game || !snap.chat,
        };
        if !need_setup {
            return;
        }

        if self.sinks.is_some() {
            log("virtual sinks lost (PipeWire restart or device change), rebuilding");
        }
        if self.prev_default.is_none() {
            // Capture before claiming; never remember our own sinks as "previous".
            self.prev_default =
                get_default_sink().filter(|d| d != GAME_SINK && d != CHAT_SINK);
        }
        match Sinks::setup(&real) {
            Ok(sinks) => {
                log(&format!("routing {GAME_SINK}/{CHAT_SINK} -> {real}"));
                self.sinks = Some(sinks);
                self.warned_no_sink = false;
                set_mix(self.last_mix.0, self.last_mix.1);
                // Claim the default unless we know the headset is off.
                if self.online != Some(false) {
                    self.claim_default();
                }
            }
            Err(e) => log(&format!("sink setup failed: {e}")),
        }
    }

    fn handle_wheel(&mut self, game: u8, chat: u8) {
        self.last_mix = (game, chat);
        if self.sinks.is_some() {
            set_mix(game, chat);
            log_verbose(&format!("chatmix: game {game}% / chat {chat}%"));
        }
    }

    fn handle_status(&mut self, st: &HeadsetStatus) {
        let is_online = st.is_online();
        match self.online {
            None => log(&format!(
                "headset {}, battery {}%",
                st.power_str(),
                st.battery_percent()
            )),
            Some(prev) if prev != is_online => {
                if is_online {
                    log(&format!(
                        "headset powered on (battery {}%), claiming default sink",
                        st.battery_percent()
                    ));
                    set_mix(self.last_mix.0, self.last_mix.1);
                    self.claim_default();
                } else {
                    log(&format!(
                        "headset {} , releasing default sink",
                        st.power_str()
                    ));
                    self.release_default();
                }
            }
            _ => {}
        }
        self.online = Some(is_online);

        if is_online && st.battery <= LOW_BATTERY_WARN && !self.battery_warned {
            let pct = st.battery_percent();
            log(&format!("LOW BATTERY: headset at {pct}%"));
            let _ = Command::new("notify-send")
                .args([
                    "-u",
                    "critical",
                    "-a",
                    "arctis-chatmix",
                    "Arctis headset battery low",
                    &format!("{pct}% remaining"),
                ])
                .status();
            self.battery_warned = true;
        } else if st.battery >= LOW_BATTERY_CLEAR || st.is_charging() {
            self.battery_warned = false;
        }
    }

    fn teardown(&mut self) {
        self.release_default();
        if self.sinks.take().is_some() {
            unload_our_modules();
            log("virtual sinks removed");
        }
    }
}

/// One connected session: init the dock, keep sinks healthy, pump wheel events
/// until the device goes away or we're told to stop.
/// Returns Ok(true) to reconnect, Ok(false) to exit.
fn run_session(hidraw: &PathBuf, manage_default: bool) -> Result<bool, String> {
    let mut dev = OpenOptions::new()
        .read(true)
        .write(true)
        .open(hidraw)
        .map_err(|e| format!("cannot open {}: {e}", hidraw.display()))?;
    let fd = dev.as_raw_fd();

    // Enable Sonar/ChatMix mode on the base station, then ask for a status
    // report (battery/power); wheel events arrive unsolicited after that.
    for cmd in INIT_COMMANDS {
        if let Err(e) = dev.write(&pad_command(cmd)) {
            return Err(format!("init command failed: {e}"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    log("sent ChatMix enable sequence");
    let status_request = pad_command(&STATUS_REQUEST);
    let _ = dev.write(&status_request);

    let mut session = Session::new(manage_default);
    session.ensure_sinks();
    let mut last_health = Instant::now();
    let mut last_status_poll = Instant::now();

    let result = loop {
        if !RUNNING.load(Ordering::SeqCst) {
            break Ok(false);
        }
        if last_health.elapsed() >= HEALTH_INTERVAL {
            session.ensure_sinks();
            last_health = Instant::now();
        }
        if last_status_poll.elapsed() >= STATUS_POLL_INTERVAL {
            if dev.write(&status_request).is_err() {
                log("device write failed, assuming disconnect");
                break Ok(true);
            }
            last_status_poll = Instant::now();
        }
        if !poll_ready(fd, 250) {
            continue;
        }

        let mut mix: Option<(u8, u8)> = None;
        let mut device_gone = false;
        // Drain everything pending so a fast wheel spin coalesces into one
        // pactl update instead of one per report.
        loop {
            let mut report = [0u8; REPORT_LEN];
            match dev.read(&mut report) {
                Ok(n) => {
                    if let Some(m) = parse_wheel(&report[..n]) {
                        mix = Some(m);
                    } else if let Some(st) = HeadsetStatus::parse(&report[..n]) {
                        session.handle_status(&st);
                    }
                }
                Err(e) => {
                    log(&format!("device read failed ({e}), assuming disconnect"));
                    device_gone = true;
                    break;
                }
            }
            if !poll_ready(fd, 0) {
                break;
            }
        }

        if let Some((game, chat)) = mix {
            session.handle_wheel(game, chat);
        }
        if device_gone {
            break Ok(true); // tear down and let main retry
        }
    };

    session.teardown();
    result
}

// ---------------------------------------------------------------------------
// `status` subcommand
// ---------------------------------------------------------------------------

fn cmd_status() -> i32 {
    let Some(hidraw) = find_hidraw() else {
        eprintln!("arctis-chatmix: base station not found (is it plugged in?)");
        return 1;
    };
    let mut dev = match OpenOptions::new().read(true).write(true).open(&hidraw) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("arctis-chatmix: cannot open {}: {e}", hidraw.display());
            return 1;
        }
    };
    if dev.write(&pad_command(&STATUS_REQUEST)).is_err() {
        eprintln!("arctis-chatmix: failed to send status request");
        return 1;
    }

    let fd = dev.as_raw_fd();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if !poll_ready(fd, 200) {
            continue;
        }
        let mut report = [0u8; REPORT_LEN];
        let Ok(n) = dev.read(&mut report) else { break };
        let Some(st) = HeadsetStatus::parse(&report[..n]) else {
            continue; // some other report (wheel etc.), keep waiting
        };
        println!("headset:      {}", st.power_str());
        println!(
            "battery:      {}%{}",
            st.battery_percent(),
            if st.is_charging() { " (charging)" } else { "" }
        );
        println!("charge slot:  {}%", st.slot_battery_percent());
        println!(
            "microphone:   {}",
            if st.mic_muted { "muted" } else { "unmuted" }
        );
        println!(
            "anc:          {} (transparency level {}/10)",
            st.anc_str(),
            st.transparent_level
        );
        println!(
            "wireless:     {} mode, {}",
            st.wireless_mode_str(),
            st.pairing_str()
        );
        println!("bluetooth:    {}", st.bluetooth_str());
        println!("auto off:     {}", st.auto_off_str());
        return 0;
    }
    eprintln!("arctis-chatmix: no status response from base station");
    1
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn usage() {
    println!(
        "arctis-chatmix — ChatMix daemon for the Arctis Nova Pro Wireless\n\
         \n\
         usage: arctis-chatmix [--verbose] [--no-default-sink]\n\
         \x20      arctis-chatmix status\n\
         \n\
         Runs as a daemon: creates the Arctis Game / Arctis Chat PipeWire sinks\n\
         and maps the base station's ChatMix wheel onto their volumes.\n\
         \n\
         status              one-shot query: battery, power, mic, ANC, ...\n\
         --no-default-sink   never touch the default sink\n\
         --verbose, -v       log every wheel adjustment\n\
         --help, -h          this text"
    );
}

fn main() {
    let mut manage_default = true;
    let mut sub_status = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "status" => sub_status = true,
            "--no-default-sink" => manage_default = false,
            "--verbose" | "-v" => VERBOSE.store(true, Ordering::Relaxed),
            "--help" | "-h" => {
                usage();
                return;
            }
            other => {
                eprintln!("arctis-chatmix: unknown argument: {other}");
                usage();
                std::process::exit(2);
            }
        }
    }
    if sub_status {
        std::process::exit(cmd_status());
    }

    install_signal_handlers();

    let mut announced_wait = false;
    while RUNNING.load(Ordering::SeqCst) {
        let Some(hidraw) = find_hidraw() else {
            if !announced_wait {
                log("waiting for Arctis Nova Pro Wireless base station...");
                announced_wait = true;
            }
            std::thread::sleep(Duration::from_secs(2));
            continue;
        };
        announced_wait = false;
        log(&format!("found base station at {}", hidraw.display()));

        match run_session(&hidraw, manage_default) {
            Ok(true) => std::thread::sleep(Duration::from_secs(2)), // reconnect
            Ok(false) => break,                                     // signal
            Err(e) => {
                log(&format!("error: {e}"));
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    }
    log("bye");
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Captured live from an Arctis Nova Pro Wireless base station.
    const REAL_UEVENT: &str = "DRIVER=hid-generic\n\
        HID_ID=0003:00001038:000012E0\n\
        HID_NAME=SteelSeries Arctis Nova Pro Wireless\n\
        HID_PHYS=usb-0000:80:14.0-10.1/input4\n\
        HID_UNIQ=\n\
        MODALIAS=hid:b0003g0001v00001038p000012E0";

    #[test]
    fn uevent_matches_real_device() {
        assert!(uevent_matches(REAL_UEVENT, &hid_ids(), "/input4"));
    }

    #[test]
    fn uevent_rejects_wrong_interface() {
        assert!(!uevent_matches(REAL_UEVENT, &hid_ids(), "/input3"));
    }

    #[test]
    fn uevent_rejects_other_device() {
        let other = REAL_UEVENT.replace("1038", "1532");
        assert!(!uevent_matches(&other, &hid_ids(), "/input4"));
    }

    #[test]
    fn wheel_report_parses() {
        let mut report = [0u8; REPORT_LEN];
        report[..4].copy_from_slice(&[0x07, 0x45, 60, 100]);
        assert_eq!(parse_wheel(&report), Some((60, 100)));
    }

    #[test]
    fn wheel_report_clamps_out_of_range() {
        let report = [0x07, 0x45, 200, 101];
        assert_eq!(parse_wheel(&report), Some((100, 100)));
    }

    #[test]
    fn wheel_rejects_other_reports() {
        assert_eq!(parse_wheel(&[0x06, 0xb0, 0, 0]), None);
        assert_eq!(parse_wheel(&[0x07, 0x25, 10, 0]), None);
        assert_eq!(parse_wheel(&[0x07]), None);
    }

    // Captured live: headset online, battery 7/8, ANC off, mic unmuted,
    // 30 min auto-off, range mode, paired+connected.
    const REAL_STATUS: [u8; 16] = [
        0x06, 0xb0, 0x00, 0x00, 0x01, 0x00, 0x07, 0x08, 0x01, 0x00, 0x00, 0x0a, 0x05, 0x01, 0x08,
        0x08,
    ];

    #[test]
    fn status_report_parses() {
        let st = HeadsetStatus::parse(&REAL_STATUS).unwrap();
        assert!(st.is_online());
        assert!(!st.is_charging());
        assert_eq!(st.battery, 7);
        assert_eq!(st.battery_percent(), 87);
        assert_eq!(st.slot_battery_percent(), 100);
        assert!(!st.mic_muted);
        assert_eq!(st.anc_str(), "off");
        assert_eq!(st.wireless_mode_str(), "range");
        assert_eq!(st.pairing_str(), "connected");
        assert_eq!(st.auto_off_str(), "30 minutes");
        assert_eq!(st.power_str(), "online");
    }

    #[test]
    fn status_battery_bounds() {
        let mut r = REAL_STATUS;
        r[6] = 0;
        assert_eq!(HeadsetStatus::parse(&r).unwrap().battery_percent(), 0);
        r[6] = 8;
        assert_eq!(HeadsetStatus::parse(&r).unwrap().battery_percent(), 100);
        r[6] = 200; // defensive clamp
        assert_eq!(HeadsetStatus::parse(&r).unwrap().battery_percent(), 100);
    }

    #[test]
    fn status_rejects_other_reports() {
        assert_eq!(HeadsetStatus::parse(&[0x07, 0x45, 50, 50]), None);
        assert_eq!(HeadsetStatus::parse(&REAL_STATUS[..8]), None);
    }

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
    fn commands_fit_report() {
        for cmd in INIT_COMMANDS {
            assert!(cmd.len() <= REPORT_LEN);
        }
        let padded = pad_command(&STATUS_REQUEST);
        assert_eq!(padded.len(), REPORT_LEN);
        assert_eq!(&padded[..2], &STATUS_REQUEST);
        assert!(padded[2..].iter().all(|&b| b == 0));
    }
}

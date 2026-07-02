//! ChatMix daemon for SteelSeries Arctis headsets.
//!
//! Creates two virtual PipeWire sinks ("Arctis Game" / "Arctis Chat") routed to the
//! headset's real sink, then listens on the device's HID interface for ChatMix
//! dial events and applies them as the two sinks' volumes. Self-heals when the
//! audio device disappears (headset off, profile disabled, PipeWire restart,
//! USB unplug, suspend/resume) and hands the default sink back while the
//! headset is powered off.
//!
//! Supported devices are described by the spec table in `devices.rs`;
//! protocol reference: https://github.com/elegos/Linux-Arctis-Manager.

mod devices;

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use devices::{frame_command, parse_report, Collected, DeviceSpec, Field, REPORT_LEN, SPECS, VENDOR_ID};

// The sink *name* doubles as the display name: Steam Game Mode's audio picker
// ignores node.nick/node.description for card-less sinks and shows the raw
// sink name, so the name itself must be presentable. pipewire-pulse accepts
// names with spaces and pactl addresses them fine.
const GAME_SINK: &str = "Arctis Game";
const CHAT_SINK: &str = "Arctis Chat";
const GAME_DESC: &str = "Arctis Game";
const CHAT_DESC: &str = "Arctis Chat";
/// Sink names used by versions before the Game-Mode rename; still matched
/// during cleanup so an upgrade tears down a crashed old instance's modules.
const LEGACY_SINKS: [&str; 2] = ["arctis_game", "arctis_chat"];

/// How often to verify the real sink and virtual sinks still exist.
const HEALTH_INTERVAL: Duration = Duration::from_secs(3);
/// Warn when the battery falls to this percentage; clear once it recovers
/// past the higher bound (or the headset is charging).
const LOW_BATTERY_WARN_PCT: u32 = 25;
const LOW_BATTERY_CLEAR_PCT: u32 = 50;

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
    println!("rust-arctis-chatmix: {msg}");
}

fn log_verbose(msg: &str) {
    if VERBOSE.load(Ordering::Relaxed) {
        log(msg);
    }
}

// ---------------------------------------------------------------------------
// HID device discovery and I/O
// ---------------------------------------------------------------------------

fn hid_ids(spec: &DeviceSpec) -> Vec<String> {
    spec.product_ids
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

/// A discovered device: the hidraw node commands are written to, plus any
/// extra read-only nodes dial events may arrive on (some models split
/// status and dial reports across USB interfaces).
struct FoundDevice {
    spec: &'static DeviceSpec,
    command: PathBuf,
    extra_listen: Vec<PathBuf>,
}

fn find_hidraw() -> Option<FoundDevice> {
    let entries: Vec<(String, String)> = fs::read_dir("/sys/class/hidraw")
        .ok()?
        .flatten()
        .filter_map(|e| {
            let content = fs::read_to_string(e.path().join("device/uevent")).ok()?;
            Some((e.file_name().to_string_lossy().into_owned(), content))
        })
        .collect();

    for (name, content) in &entries {
        for spec in SPECS {
            let ids = hid_ids(spec);
            if !uevent_matches(content, &ids, &format!("/input{}", spec.command_interface)) {
                continue;
            }
            // Sibling interfaces of the *same physical device*: same
            // HID_PHYS up to the trailing /input<N>.
            let phys_base = content
                .lines()
                .find_map(|l| l.strip_prefix("HID_PHYS="))
                .and_then(|p| p.rsplit_once("/input"))
                .map(|(base, _)| base.to_string())
                .unwrap_or_default();
            let extra_listen = spec
                .extra_listen_interfaces
                .iter()
                .filter_map(|iface| {
                    let suffix = format!("{phys_base}/input{iface}");
                    entries
                        .iter()
                        .find(|(n, c)| {
                            n != name
                                && uevent_matches(c, &ids, &suffix)
                        })
                        .map(|(n, _)| PathBuf::from("/dev").join(n))
                })
                .collect();
            return Some(FoundDevice {
                spec,
                command: PathBuf::from("/dev").join(name),
                extra_listen,
            });
        }
    }
    None
}

/// Write a command with the spec's report framing. Some devices' framing is
/// untested: if the kernel rejects the report id layout (EINVAL), flip
/// between numbered/unnumbered framing once and remember what worked.
fn write_command(dev: &mut File, cmd: &[u8], unnumbered: &mut bool) -> std::io::Result<()> {
    match dev.write(&frame_command(cmd, *unnumbered)) {
        Ok(_) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {
            *unnumbered = !*unnumbered;
            log(&format!(
                "hidraw rejected write, switching to {} report framing",
                if *unnumbered { "unnumbered" } else { "numbered" }
            ));
            dev.write(&frame_command(cmd, *unnumbered)).map(|_| ())
        }
        Err(e) => Err(e),
    }
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

/// True if any of the fds has data or an error pending.
fn poll_any(fds: &[i32], timeout_ms: i32) -> bool {
    let mut pfds: Vec<libc::pollfd> = fds
        .iter()
        .map(|&fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, timeout_ms) > 0 }
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

fn set_mix(game: u8, chat: u8) {
    let _ = pactl(&["set-sink-volume", GAME_SINK, &format!("{}%", game.min(100))]);
    let _ = pactl(&["set-sink-volume", CHAT_SINK, &format!("{}%", chat.min(100))]);
}

// ---------------------------------------------------------------------------
// Daemon session
// ---------------------------------------------------------------------------

struct Session {
    spec: &'static DeviceSpec,
    sinks: Option<Sinks>,
    /// Default sink before we claimed it; restored when the headset powers
    /// off and on exit.
    prev_default: Option<String>,
    manage_default: bool,
    last_mix: (u8, u8),
    /// None until the first status report carrying a Power field.
    online: Option<bool>,
    battery_pct: Option<u32>,
    battery_warned: bool,
    warned_no_sink: bool,
}

impl Session {
    fn new(spec: &'static DeviceSpec, manage_default: bool) -> Self {
        Self {
            spec,
            sinks: None,
            prev_default: None,
            manage_default,
            last_mix: (100, 100),
            online: None,
            battery_pct: None,
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
        let Some(snap) = snapshot_sinks(self.spec) else {
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

    fn handle_mix(&mut self, game: u8, chat: u8) {
        if (game, chat) == self.last_mix {
            return;
        }
        self.last_mix = (game, chat);
        if self.sinks.is_some() {
            set_mix(game, chat);
            log_verbose(&format!("chatmix: game {game}% / chat {chat}%"));
        }
    }

    fn battery_str(&self) -> String {
        match self.battery_pct {
            Some(pct) => format!("battery {pct}%"),
            None => "battery unknown".to_string(),
        }
    }

    fn apply_status(&mut self, fields: &Collected) {
        if let Some(b) = fields.get(Field::Battery) {
            self.battery_pct = Some(self.spec.battery_percent(b));
        }
        let charging = self.spec.is_charging(fields);

        if let Some(p) = fields.get(Field::Power) {
            let is_online = self.spec.is_online(p);
            let power_str = (self.spec.describe)(fields, Field::Power)
                .unwrap_or_else(|| format!("power state {p:#04x}"));
            match self.online {
                None => log(&format!("headset {}, {}", power_str, self.battery_str())),
                Some(prev) if prev != is_online => {
                    if is_online {
                        log(&format!(
                            "headset powered on ({}), claiming default sink",
                            self.battery_str()
                        ));
                        set_mix(self.last_mix.0, self.last_mix.1);
                        self.claim_default();
                    } else {
                        log(&format!("headset {power_str}, releasing default sink"));
                        self.release_default();
                    }
                }
                _ => {}
            }
            self.online = Some(is_online);
        }

        if let Some(pct) = self.battery_pct {
            if self.online != Some(false) && pct <= LOW_BATTERY_WARN_PCT && !self.battery_warned {
                log(&format!("LOW BATTERY: headset at {pct}%"));
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
                self.battery_warned = true;
            } else if pct >= LOW_BATTERY_CLEAR_PCT || charging {
                self.battery_warned = false;
            }
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

/// One connected session: init the device, keep sinks healthy, pump ChatMix
/// events until the device goes away or we're told to stop.
/// Returns Ok(true) to reconnect, Ok(false) to exit.
fn run_session(found: &FoundDevice, manage_default: bool) -> Result<bool, String> {
    let spec = found.spec;
    let mut dev = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&found.command)
        .map_err(|e| format!("cannot open {}: {e}", found.command.display()))?;
    let mut extras: Vec<File> = Vec::new();
    for path in &found.extra_listen {
        match OpenOptions::new().read(true).open(path) {
            Ok(f) => extras.push(f),
            Err(e) => log(&format!("cannot open {} (ignoring): {e}", path.display())),
        }
    }
    let all_fds: Vec<i32> = std::iter::once(dev.as_raw_fd())
        .chain(extras.iter().map(|f| f.as_raw_fd()))
        .collect();
    let mut unnumbered = spec.unnumbered_reports;

    // Enable ChatMix mode where the device needs it, then ask for a status
    // report; dial events arrive unsolicited after that.
    for cmd in spec.init {
        if let Err(e) = write_command(&mut dev, cmd, &mut unnumbered) {
            return Err(format!("init command failed: {e}"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if !spec.init.is_empty() {
        log("sent init sequence");
    }
    for req in spec.status_requests {
        let _ = write_command(&mut dev, req, &mut unnumbered);
    }
    if !spec.has_chatmix() {
        log(&format!(
            "note: no ChatMix protocol is documented for the {}; \
             providing sinks and status only",
            spec.name
        ));
    }

    let mut session = Session::new(spec, manage_default);
    session.ensure_sinks();
    let mut last_health = Instant::now();
    let mut last_status_poll = Instant::now();
    let status_poll = Duration::from_secs(spec.status_poll_secs);

    let result = loop {
        if !RUNNING.load(Ordering::SeqCst) {
            break Ok(false);
        }
        if last_health.elapsed() >= HEALTH_INTERVAL {
            session.ensure_sinks();
            last_health = Instant::now();
        }
        if spec.has_status() && last_status_poll.elapsed() >= status_poll {
            let mut failed = false;
            for req in spec.status_requests {
                if write_command(&mut dev, req, &mut unnumbered).is_err() {
                    failed = true;
                }
            }
            if failed {
                log("device write failed, assuming disconnect");
                break Ok(true);
            }
            last_status_poll = Instant::now();
        }
        if !poll_any(&all_fds, 250) {
            continue;
        }

        let mut mix: Option<(u8, u8)> = None;
        let mut device_gone = false;
        // Drain everything pending on every node so a fast dial spin
        // coalesces into one pactl update instead of one per report.
        for file in std::iter::once(&mut dev).chain(extras.iter_mut()) {
            let fd = file.as_raw_fd();
            while poll_ready(fd, 0) {
                let mut report = [0u8; REPORT_LEN + 1];
                match file.read(&mut report) {
                    Ok(n) => {
                        let fields = parse_report(spec, &report[..n]);
                        if let (Some(g), Some(c)) =
                            (fields.get(Field::GameMix), fields.get(Field::ChatMix))
                        {
                            mix = Some((g.min(100), c.min(100)));
                        }
                        if fields.get(Field::Power).is_some()
                            || fields.get(Field::Battery).is_some()
                        {
                            session.apply_status(&fields);
                        }
                    }
                    Err(e) => {
                        log(&format!("device read failed ({e}), assuming disconnect"));
                        device_gone = true;
                        break;
                    }
                }
            }
            if device_gone {
                break;
            }
        }

        if let Some((game, chat)) = mix {
            session.handle_mix(game, chat);
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

fn cmd_status() -> i32 {
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

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn usage() {
    println!(
        "rust-arctis-chatmix — ChatMix daemon for SteelSeries Arctis headsets\n\
         \n\
         usage: rust-arctis-chatmix [--verbose] [--no-default-sink]\n\
         \x20      rust-arctis-chatmix status\n\
         \n\
         Runs as a daemon: creates the Arctis Game / Arctis Chat PipeWire sinks\n\
         and maps the device's ChatMix dial onto their volumes.\n\
         \n\
         Supported: Nova Pro Wireless, Nova Pro (GameDAC Gen 2), Nova 7 family,\n\
         Arctis 7+, Nova Elite, Nova 5 (sinks/status only). Only the Nova Pro\n\
         Wireless has been verified on real hardware.\n\
         \n\
         status              one-shot query: battery, power, mic, ...\n\
         --no-default-sink   never touch the default sink\n\
         --verbose, -v       log every dial adjustment\n\
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
                eprintln!("rust-arctis-chatmix: unknown argument: {other}");
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
        let Some(found) = find_hidraw() else {
            if !announced_wait {
                log("waiting for a supported Arctis device...");
                announced_wait = true;
            }
            std::thread::sleep(Duration::from_secs(2));
            continue;
        };
        announced_wait = false;
        log(&format!(
            "found {} at {}{}",
            found.spec.name,
            found.command.display(),
            if found.extra_listen.is_empty() {
                String::new()
            } else {
                format!(
                    " (+{} listen node{})",
                    found.extra_listen.len(),
                    if found.extra_listen.len() == 1 { "" } else { "s" }
                )
            }
        ));

        match run_session(&found, manage_default) {
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
    use devices::NOVA_PRO_WIRELESS;

    // Captured live from an Arctis Nova Pro Wireless base station.
    const REAL_UEVENT: &str = "DRIVER=hid-generic\n\
        HID_ID=0003:00001038:000012E0\n\
        HID_NAME=SteelSeries Arctis Nova Pro Wireless\n\
        HID_PHYS=usb-0000:80:14.0-10.1/input4\n\
        HID_UNIQ=\n\
        MODALIAS=hid:b0003g0001v00001038p000012E0";

    #[test]
    fn uevent_matches_real_device() {
        assert!(uevent_matches(REAL_UEVENT, &hid_ids(&NOVA_PRO_WIRELESS), "/input4"));
    }

    #[test]
    fn uevent_rejects_wrong_interface() {
        assert!(!uevent_matches(REAL_UEVENT, &hid_ids(&NOVA_PRO_WIRELESS), "/input3"));
    }

    #[test]
    fn uevent_rejects_other_device() {
        let other = REAL_UEVENT.replace("1038", "1532");
        assert!(!uevent_matches(&other, &hid_ids(&NOVA_PRO_WIRELESS), "/input4"));
    }

    #[test]
    fn uevent_matches_only_its_own_spec() {
        // A Nova 7 on interface 3 must not match the Nova Pro spec and
        // vice versa.
        let nova7 = REAL_UEVENT
            .replace("12E0", "22A1")
            .replace("input4", "input3");
        assert!(!uevent_matches(&nova7, &hid_ids(&NOVA_PRO_WIRELESS), "/input4"));
        assert!(uevent_matches(
            &nova7,
            &hid_ids(&devices::NOVA_7_PERCENT),
            "/input3"
        ));
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
}

//! The daemon session: device init, the poll/parse event loop, sink health
//! checks, default-sink claim/release, and low-battery warnings.

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::audio::{
    get_default_sink, pactl, set_mix, snapshot_sinks, unload_our_modules, Sinks, CHAT_SINK,
    GAME_SINK,
};
use crate::devices::{parse_report, Collected, DeviceSpec, Field, REPORT_LEN};
use crate::hid::{poll_any, poll_ready, write_command, FoundDevice};
use crate::{log, log_verbose, RUNNING};

/// How often to verify the real sink and virtual sinks still exist.
const HEALTH_INTERVAL: Duration = Duration::from_secs(3);
/// Warn when the battery falls to this percentage; clear once it recovers
/// past the higher bound (or the headset is charging).
const LOW_BATTERY_WARN_PCT: u32 = 25;
const LOW_BATTERY_CLEAR_PCT: u32 = 50;

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
pub(crate) fn run_session(found: &FoundDevice, manage_default: bool) -> Result<bool, String> {
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

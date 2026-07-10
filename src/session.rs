//! The daemon session: device init, the poll/parse event loop, sink health
//! checks, default-sink claim/release, and low-battery warnings.

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::audio::{Audio, Pactl, SinkWatch, WatchEvent, CHAT_SINK, GAME_SINK};
use crate::devices::{parse_report, Collected, DeviceSpec, Field, REPORT_LEN};
use crate::hid::{poll_any, poll_ready, write_command, FoundDevice};
use crate::{log, log_verbose, RUNNING};

/// How often to verify the real sink and virtual sinks still exist when
/// `pactl subscribe` isn't available, and how often to retry spawning it.
const HEALTH_INTERVAL: Duration = Duration::from_secs(3);
/// With a live event stream the tick is only a safety net.
const HEALTH_INTERVAL_EVENTED: Duration = Duration::from_secs(30);
/// Warn when the battery falls to this percentage; clear once it recovers
/// past the higher bound (or the headset is charging).
const LOW_BATTERY_WARN_PCT: u32 = 25;
const LOW_BATTERY_CLEAR_PCT: u32 = 50;

struct Session<'a> {
    spec: &'static DeviceSpec,
    audio: &'a dyn Audio,
    /// `Some(real_sink)` while our virtual sinks exist, routed into it.
    sinks: Option<String>,
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

impl<'a> Session<'a> {
    fn new(spec: &'static DeviceSpec, manage_default: bool, audio: &'a dyn Audio) -> Self {
        Self {
            spec,
            audio,
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
            let _ = self.audio.set_default_sink(GAME_SINK);
        }
    }

    fn release_default(&self) {
        if !self.manage_default {
            return;
        }
        // Only hand it back if we still hold it — don't stomp on a choice
        // the user made in the meantime.
        let current = self.audio.default_sink();
        if !matches!(current.as_deref(), Some(GAME_SINK) | Some(CHAT_SINK)) {
            return;
        }
        if let Some(prev) = &self.prev_default {
            if self.audio.set_default_sink(prev).is_ok() {
                return;
            }
        }
        if let Some(real) = &self.sinks {
            let _ = self.audio.set_default_sink(real);
        }
    }

    /// Health check: make sure the real sink exists and our virtual sinks are
    /// alive, (re)creating them as needed. Handles first-time setup, the audio
    /// device disappearing (profile off / unplug), and PipeWire restarts.
    fn ensure_sinks(&mut self) {
        let Some(snap) = self.audio.snapshot(self.spec) else {
            return; // pactl unavailable right now (pipewire restarting); retry next tick
        };

        let Some(real) = snap.real else {
            if self.sinks.take().is_some() {
                log("Arctis audio sink disappeared, removing virtual sinks");
                self.audio.teardown_sinks();
            } else if !self.warned_no_sink {
                log("waiting for Arctis audio sink...");
                self.warned_no_sink = true;
            }
            return;
        };

        let need_setup = match &self.sinks {
            None => true,
            Some(s) => *s != real || !snap.game || !snap.chat,
        };
        if !need_setup {
            return;
        }

        if self.sinks.is_some() {
            log("virtual sinks lost (PipeWire restart or device change), rebuilding");
        }
        if self.prev_default.is_none() {
            // Capture before claiming; never remember our own sinks as "previous".
            self.prev_default = self
                .audio
                .default_sink()
                .filter(|d| d != GAME_SINK && d != CHAT_SINK);
        }
        match self.audio.setup_sinks(&real) {
            Ok(()) => {
                log(&format!("routing {GAME_SINK}/{CHAT_SINK} -> {real}"));
                self.sinks = Some(real);
                self.warned_no_sink = false;
                self.audio.set_mix(self.last_mix.0, self.last_mix.1);
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
            self.audio.set_mix(game, chat);
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
                        self.audio.set_mix(self.last_mix.0, self.last_mix.1);
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
                self.audio.notify_low_battery(pct);
                self.battery_warned = true;
            } else if pct >= LOW_BATTERY_CLEAR_PCT || charging {
                self.battery_warned = false;
            }
        }
    }

    fn teardown(&mut self) {
        self.release_default();
        if self.sinks.take().is_some() {
            self.audio.teardown_sinks();
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
    let dev_fds: Vec<i32> = std::iter::once(dev.as_raw_fd())
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

    let audio = Pactl;
    let mut session = Session::new(spec, manage_default, &audio);
    session.ensure_sinks();

    // Sink changes arrive as events; the health tick is a slow safety net
    // (and the polling fallback when the event stream isn't available).
    let mut watch = SinkWatch::spawn();
    if watch.is_none() {
        log("pactl subscribe unavailable, falling back to sink polling");
    }
    let mut last_health = Instant::now();
    let mut last_status_poll = Instant::now();
    let status_poll = Duration::from_secs(spec.status_poll_secs);

    let result = loop {
        if !RUNNING.load(Ordering::SeqCst) {
            break Ok(false);
        }
        let health_interval = if watch.is_some() {
            HEALTH_INTERVAL_EVENTED
        } else {
            HEALTH_INTERVAL
        };
        if last_health.elapsed() >= health_interval {
            session.ensure_sinks();
            last_health = Instant::now();
            if watch.is_none() {
                // PipeWire may be back after a restart; resubscribe.
                watch = SinkWatch::spawn();
            }
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

        let mut fds = dev_fds.clone();
        if let Some(w) = &watch {
            fds.push(w.fd());
        }
        if !poll_any(&fds, 250) {
            continue;
        }

        if let Some(w) = &mut watch {
            match w.drain() {
                WatchEvent::Changed => {
                    session.ensure_sinks();
                    last_health = Instant::now();
                }
                WatchEvent::Quiet => {}
                WatchEvent::Ended => {
                    log("PipeWire event stream ended, falling back to sink polling");
                    watch = None;
                }
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::SinkSnapshot;
    use crate::devices::NOVA_PRO_WIRELESS;
    use std::cell::RefCell;

    /// Records every audio operation; snapshot and default sink are
    /// scriptable. `set_default_sink` updates the fake default, mirroring
    /// the real server.
    #[derive(Default)]
    struct FakeAudio {
        calls: RefCell<Vec<String>>,
        snapshot: RefCell<Option<SinkSnapshot>>,
        default: RefCell<Option<String>>,
    }

    impl FakeAudio {
        fn set_snapshot(&self, game: bool, chat: bool, real: Option<&str>) {
            *self.snapshot.borrow_mut() = Some(SinkSnapshot {
                game,
                chat,
                real: real.map(str::to_string),
            });
        }

        fn set_default(&self, sink: &str) {
            *self.default.borrow_mut() = Some(sink.to_string());
        }

        fn take_calls(&self) -> Vec<String> {
            self.calls.replace(Vec::new())
        }
    }

    impl Audio for FakeAudio {
        fn snapshot(&self, _spec: &DeviceSpec) -> Option<SinkSnapshot> {
            self.snapshot.borrow().clone()
        }

        fn setup_sinks(&self, real_sink: &str) -> Result<(), String> {
            self.calls.borrow_mut().push(format!("setup:{real_sink}"));
            Ok(())
        }

        fn teardown_sinks(&self) {
            self.calls.borrow_mut().push("teardown".into());
        }

        fn set_mix(&self, game: u8, chat: u8) {
            self.calls.borrow_mut().push(format!("mix:{game}/{chat}"));
        }

        fn default_sink(&self) -> Option<String> {
            self.default.borrow().clone()
        }

        fn set_default_sink(&self, sink: &str) -> Result<(), String> {
            self.calls.borrow_mut().push(format!("default:{sink}"));
            *self.default.borrow_mut() = Some(sink.to_string());
            Ok(())
        }

        fn notify_low_battery(&self, pct: u32) {
            self.calls.borrow_mut().push(format!("notify:{pct}"));
        }
    }

    fn status(power: Option<u8>, battery: Option<u8>) -> Collected {
        let mut c = Collected::default();
        if let Some(p) = power {
            c.insert(Field::Power, p);
        }
        if let Some(b) = battery {
            c.insert(Field::Battery, b);
        }
        c
    }

    /// A session with sinks built: real sink "alsa_out", previous default
    /// "speakers", default claimed.
    fn established(audio: &FakeAudio) -> Session<'_> {
        audio.set_snapshot(false, false, Some("alsa_out"));
        audio.set_default("speakers");
        let mut s = Session::new(&NOVA_PRO_WIRELESS, true, audio);
        s.ensure_sinks();
        audio.take_calls();
        s
    }

    // NOVA_PRO_WIRELESS semantics used below: battery_max 8 (raw 2 = 25%,
    // raw 4 = 50%), power 0x08 = online, 0x00 = offline.

    #[test]
    fn first_setup_claims_default_and_replays_mix() {
        let audio = FakeAudio::default();
        audio.set_snapshot(false, false, Some("alsa_out"));
        audio.set_default("speakers");
        let mut s = Session::new(&NOVA_PRO_WIRELESS, true, &audio);
        s.ensure_sinks();
        assert_eq!(
            audio.take_calls(),
            ["setup:alsa_out", "mix:100/100", "default:Arctis Game"]
        );
        // The user's sink was remembered, not ours.
        assert_eq!(s.prev_default.as_deref(), Some("speakers"));
    }

    #[test]
    fn healthy_sinks_are_left_alone() {
        let audio = FakeAudio::default();
        let mut s = established(&audio);
        audio.set_snapshot(true, true, Some("alsa_out"));
        s.ensure_sinks();
        assert!(audio.take_calls().is_empty());
    }

    #[test]
    fn missing_virtual_sink_triggers_rebuild() {
        let audio = FakeAudio::default();
        let mut s = established(&audio);
        audio.set_snapshot(true, false, Some("alsa_out")); // chat sink lost
        s.ensure_sinks();
        assert!(audio.take_calls().contains(&"setup:alsa_out".to_string()));
    }

    #[test]
    fn real_sink_change_triggers_rebuild() {
        let audio = FakeAudio::default();
        let mut s = established(&audio);
        audio.set_snapshot(true, true, Some("other_out"));
        s.ensure_sinks();
        assert!(audio.take_calls().contains(&"setup:other_out".to_string()));
        assert_eq!(s.sinks.as_deref(), Some("other_out"));
    }

    #[test]
    fn vanished_real_sink_tears_down_once() {
        let audio = FakeAudio::default();
        let mut s = established(&audio);
        audio.set_snapshot(false, false, None);
        s.ensure_sinks();
        assert_eq!(audio.take_calls(), ["teardown"]);
        s.ensure_sinks(); // still gone: no repeated teardown
        assert!(audio.take_calls().is_empty());
    }

    #[test]
    fn power_off_restores_previous_default() {
        let audio = FakeAudio::default();
        let mut s = established(&audio);
        s.apply_status(&status(Some(0x08), Some(8))); // first report: online
        audio.take_calls();
        s.apply_status(&status(Some(0x00), None)); // powered off
        assert_eq!(audio.take_calls(), ["default:speakers"]);
    }

    #[test]
    fn release_respects_a_manual_default_change() {
        let audio = FakeAudio::default();
        let mut s = established(&audio);
        s.apply_status(&status(Some(0x08), Some(8)));
        audio.take_calls();
        audio.set_default("usb-dac"); // user picked something else meanwhile
        s.apply_status(&status(Some(0x00), None));
        assert!(audio.take_calls().is_empty());
        assert_eq!(audio.default_sink().as_deref(), Some("usb-dac"));
    }

    #[test]
    fn power_on_reclaims_default_and_replays_mix() {
        let audio = FakeAudio::default();
        let mut s = established(&audio);
        s.apply_status(&status(Some(0x08), Some(8)));
        s.handle_mix(60, 80);
        s.apply_status(&status(Some(0x00), None));
        audio.take_calls();
        s.apply_status(&status(Some(0x08), None)); // powered back on
        assert_eq!(audio.take_calls(), ["mix:60/80", "default:Arctis Game"]);
    }

    #[test]
    fn disabled_default_management_never_touches_default() {
        let audio = FakeAudio::default();
        audio.set_snapshot(false, false, Some("alsa_out"));
        audio.set_default("speakers");
        let mut s = Session::new(&NOVA_PRO_WIRELESS, false, &audio);
        s.ensure_sinks();
        s.apply_status(&status(Some(0x08), Some(8)));
        s.apply_status(&status(Some(0x00), None));
        s.apply_status(&status(Some(0x08), None));
        s.teardown();
        assert!(!audio
            .take_calls()
            .iter()
            .any(|c| c.starts_with("default:")));
    }

    #[test]
    fn low_battery_warns_once_with_hysteresis() {
        let audio = FakeAudio::default();
        let mut s = established(&audio);
        s.apply_status(&status(Some(0x08), Some(8))); // 100%: no warning
        audio.take_calls();

        s.apply_status(&status(None, Some(2))); // 25%: warn
        assert_eq!(audio.take_calls(), ["notify:25"]);
        s.apply_status(&status(None, Some(2))); // still 25%: no repeat
        assert!(audio.take_calls().is_empty());

        s.apply_status(&status(None, Some(3))); // 37%: below the clear bound, stays latched
        assert!(audio.take_calls().is_empty());
        s.apply_status(&status(None, Some(2))); // dipping again without recovery: silent
        assert!(audio.take_calls().is_empty());

        s.apply_status(&status(None, Some(4))); // 50%: re-arms the warning
        s.apply_status(&status(None, Some(2))); // 25% again: warns again
        assert_eq!(audio.take_calls(), ["notify:25"]);
    }

    #[test]
    fn no_battery_warning_while_powered_off() {
        let audio = FakeAudio::default();
        let mut s = established(&audio);
        s.apply_status(&status(Some(0x00), Some(1))); // off, 12%
        assert!(!audio.take_calls().iter().any(|c| c.starts_with("notify:")));
    }

    #[test]
    fn teardown_releases_default_and_unloads() {
        let audio = FakeAudio::default();
        let mut s = established(&audio);
        s.teardown();
        assert_eq!(audio.take_calls(), ["default:speakers", "teardown"]);
        assert!(s.sinks.is_none());
    }
}

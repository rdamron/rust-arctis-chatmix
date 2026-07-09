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
//!
//! Module map: `hid` (discovery + raw I/O), `audio` (PipeWire via pactl),
//! `session` (the daemon state machine), `status` (one-shot subcommand).

mod audio;
mod devices;
mod hid;
mod session;
mod status;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use hid::find_hidraw;
use session::run_session;
use status::cmd_status;

pub(crate) static RUNNING: AtomicBool = AtomicBool::new(true);
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

pub(crate) fn log(msg: &str) {
    println!("rust-arctis-chatmix: {msg}");
}

pub(crate) fn log_verbose(msg: &str) {
    if VERBOSE.load(Ordering::Relaxed) {
        log(msg);
    }
}

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

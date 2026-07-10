# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A ChatMix daemon for SteelSeries Arctis headsets: creates two virtual PipeWire
sinks (`Arctis Game`/`Arctis Chat`), routes them into the headset's real sink,
and drives their volumes from the headset's ChatMix dial via raw hidraw I/O.
Ships as a single static binary + systemd user service. Protocols are
translated from [Linux-Arctis-Manager](https://github.com/elegos/Linux-Arctis-Manager)'s
`devices/*.yaml` (hence the GPL-3.0 license — keep it).

## Build, test, deploy

Always build for musl — the binary runs on an atomic host (Bazzite) whose
glibc may be older than the build container's:

```sh
cargo test  --target x86_64-unknown-linux-musl
cargo test  --target x86_64-unknown-linux-musl nova_7        # single test by name filter
cargo build --release --target x86_64-unknown-linux-musl
```

Deploying a change to this machine:

```sh
./install.sh        # builds, installs to ~/.local/bin, (re)starts the systemd user service
```

## Publishing a release

Toolchain-less installs download the latest GitHub Release asset
(`releases/latest/download/rust-arctis-chatmix`), built by CI from the tag —
there is no binary in the repo. To publish:

1. Bump `version` in `Cargo.toml`, commit (Cargo.lock updates on build).
2. Push, then tag and push the tag — **pushes must go through the host**
   (the container has no git credentials):

   ```sh
   distrobox-host-exec git -C /var/home/aj/Projects/rust-arctis-chatmix push
   git tag -a v0.x.y -m "summary of the release"
   distrobox-host-exec git -C /var/home/aj/Projects/rust-arctis-chatmix push origin v0.x.y
   ```

3. The `Release` workflow (`.github/workflows/release.yml`) tests, builds the
   musl binary, and creates the release with the binary attached and
   auto-generated notes. Nothing is uploaded manually.
4. Verify with plain curl (no `gh` on host or container):
   `curl -s https://api.github.com/repos/rdamron/rust-arctis-chatmix/releases/latest`
   — check `tag_name` and that `assets` lists `rust-arctis-chatmix`.

`install.sh` detects distrobox/toolbox and routes `systemctl` through
`distrobox-host-exec` automatically. From inside a container, inspect the
service the same way:

```sh
distrobox-host-exec systemctl --user status rust-arctis-chatmix
distrobox-host-exec journalctl --user -u rust-arctis-chatmix -n 20 --no-pager
rust-arctis-chatmix status        # one-shot device query; works in-container
```

`pactl` and `/dev/hidraw*` are reachable from inside the container, so live
debugging (reading raw HID reports, checking sinks) does not need the host.

## Architecture

See `ARCHITECTURE.md` for the full flow. Module map:

- `src/devices.rs` — declarative `DeviceSpec` table, one per headset family.
  A spec describes: USB product ids, which USB interface takes commands and
  which extra interfaces to read (`extra_listen_interfaces`), HID report
  framing, an init handshake, status-request bytes, and `Frame`s (report
  prefix + byte offsets of `Field`s like GameMix/Battery/Power). Semantics
  (battery scale, which power values mean "online"/"charging") and a
  `describe` fn for human-readable status output are also per-spec.
- `src/hid.rs` — hidraw discovery (sysfs uevent matching) and raw I/O:
  `find_hidraw`, `write_command` (with the framing fallback), poll helpers.
- `src/audio.rs` — PipeWire plumbing via `pactl` subprocess calls: sink
  names, virtual sink setup/teardown, sink snapshots, volumes, default sink.
- `src/session.rs` — the daemon state machine: `run_session` (init → poll
  fds → parse reports → apply mix/status), default-sink claim/release,
  self-healing health checks, low-battery warnings.
- `src/status.rs` — the one-shot `status` subcommand.
- `src/main.rs` — arg parsing, logging, signals, and the outer
  discovery/retry loop.

The daemon is deliberately dumb about state: a health check re-checks that
sinks exist and rebuilds them (PipeWire restarts, device unplug, profile
toggles) — triggered by `pactl subscribe` events with a 30s safety tick, or
3s polling when subscribe is unavailable — and a device disconnect just
tears down the session and re-enters the discovery loop. `Session` talks to
PipeWire through the `Audio` trait so its state machine is unit-testable
(`session::tests` against a fake).

Design rule: init sequences must never include upstream commands that
overwrite on-device settings (EQ, sidetone, auto-off, wireless mode...) —
only queries and the Sonar/ChatMix enables. Users manage settings from the
device itself.

## Hardware truth vs. upstream YAMLs

Only two families are verified on real hardware: **Nova Pro Wireless**
(1038:12e0) and **Nova 7X Gen 2** (1038:229e). The rest are faithful YAML
translations that have never touched hardware — treat their specs as
hypotheses, and don't "fix" them speculatively without a tester.

Empirical findings that override upstream documentation:

- Nova 7 Gen 2 streams dial events (`45 <game> <chat>`) on USB **interface
  5**; interface 3 (where upstream says to listen) only carries the polled
  `b0` status frame. Hence `extra_listen_interfaces`.
- Nova 7 Gen 2 reports battery as a plain **0-100 percent**, not the 0-128
  upstream claims (a full 7X Gen 2 sends `0x64` in the `b0` frame — the old
  128 divisor capped the display at 78%).
- Interface-3 devices (Nova 7/7+/5) use unnumbered HID reports: hidraw
  writes need a 0x00 report-id prefix (`frame_command`), and reads come back
  as bare payload. The Nova Pro family and Elite use numbered reports
  (report id is the first buffer byte both directions). `write_command` has
  a runtime EINVAL fallback that flips framing if a spec's guess is wrong.
- Frame byte offsets are relative to the hidraw read buffer, which differs
  between numbered/unnumbered interfaces — upstream YAML offsets already
  account for this; copy them verbatim.

## Environment gotchas

- `pactl -f json list modules` omits module indices — parse
  `pactl list modules short` (tab-separated) to unload modules.
- Steam Game Mode's audio picker (`CSystemAudioController` in steamui.so,
  closed source; gamescope has no audio code) shows the card description for
  hardware sinks, but for card-less sinks (module-null-sink has no card) it
  shows the raw sink name and ignores `node.nick`/`node.description` entirely.
  Hence the sink *names* are the display strings "Arctis Game"/"Arctis Chat" —
  pipewire-pulse accepts names with spaces (they must be quoted inside module
  argument strings) and pactl addresses them fine. `node.description` is still
  set for desktop tools.
- The dev container has no `lsusb`; enumerate USB via
  `/sys/bus/usb/devices/*/idVendor` + `idProduct`, and hidraw nodes via
  `/sys/class/hidraw/*/device/uevent` (HID_ID has vendor/product, HID_PHYS
  ends in `/input<usb interface>`).
- Multiple processes can read the same `/dev/hidraw*` concurrently — a
  debug capture script can run alongside the daemon without stealing
  reports.

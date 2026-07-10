# Architecture

`rust-arctis-chatmix` is a single-binary daemon that gives SteelSeries Arctis
headsets a working ChatMix dial on Linux/PipeWire. It creates two virtual
sinks — **Arctis Game** and **Arctis Chat** — loops both into the headset's
real sink, and maps the hardware dial onto the two sinks' volumes by talking
to the headset directly over hidraw.

There is no persistent state, no config file, and no IPC. The daemon derives
everything from the environment on every tick and self-heals when the
environment changes (headset off, USB unplug, PipeWire restart, suspend).

## Process modes

The binary runs in one of two modes, chosen by the command line:

- **Daemon** (default): the long-running service described below.
  `--no-default-sink` disables default-sink management; `--verbose` logs each
  dial movement.
- **`status` subcommand**: a one-shot query. Finds the device, sends the
  spec's status requests, collects reply frames until the device goes quiet
  (some models spread status over several frames), and prints a labelled
  report (battery, power, mic, ANC, ...). It exits without touching audio,
  and can run alongside the daemon — hidraw allows concurrent readers.

## The two halves

### `src/devices.rs` — what to say to each headset

A declarative table of `DeviceSpec`s, one per headset family, translated from
[Linux-Arctis-Manager](https://github.com/elegos/Linux-Arctis-Manager)'s
`devices/*.yaml`. A spec is pure data plus a couple of tiny interpretation
helpers; it contains **no I/O**:

- **Identity**: USB product ids (vendor is always 0x1038), which USB
  interface accepts commands (`command_interface`), and any extra interfaces
  that must also be read because dial events arrive there
  (`extra_listen_interfaces` — e.g. Nova 7 Gen 2 streams dial events on
  interface 5 while status lives on interface 3).
- **Report framing**: `unnumbered_reports` says whether the interface uses
  HID report ids. Unnumbered interfaces need a `0x00` prefix on writes
  (`frame_command`) and return bare payloads on reads; numbered interfaces
  carry the report id as the first byte both ways.
- **Protocol bytes**: an `init` handshake (ChatMix/Sonar enables only — by
  design never anything that overwrites on-device settings), and
  `status_requests` re-sent every `status_poll_secs`.
- **`Frame`s**: how to parse inbound reports. Each frame is a byte prefix to
  match plus `(Field, offset)` pairs — GameMix, ChatMix, Battery, Power, and
  friends. `parse_report` matches a report against the spec's frames and
  returns a `Collected` bag of field values.
- **Semantics**: battery scale (`battery_max`), which Power values mean
  online/charging, and a per-spec `describe` fn that renders raw field bytes
  as human-readable strings for `status` output.

Only the Nova Pro Wireless and Nova 7X Gen 2 specs are verified on hardware;
the rest are faithful YAML translations (see CLAUDE.md for the empirical
corrections that override upstream).

### The rest — everything that touches the machine

One module per responsibility:

1. **`src/hid.rs`** — HID discovery and I/O: find the device and move bytes.
2. **`src/audio.rs`** — PipeWire plumbing: create/destroy sinks and set
   volumes via `pactl`.
3. **`src/session.rs`** — the state machine tying 1 and 2 together.
4. **`src/status.rs`** — the one-shot `status` subcommand.
5. **`src/main.rs`** — entry points: arg parsing, logging, signal handling,
   and the outer discovery/retry loop.

## Daemon flow

```
main
 │  parse args, install SIGINT/SIGTERM handler (sets RUNNING=false)
 ▼
discovery loop ◄──────────────────────────────────────────────┐
 │  find_hidraw(): scan /sys/class/hidraw/*/device/uevent,    │
 │  match HID_ID (vendor:product) + HID_PHYS ("/input<N>")    │
 │  against every DeviceSpec; also resolve sibling            │
 │  extra_listen interfaces of the same physical device.      │
 │  No match → sleep 2 s, rescan.                             │
 ▼                                                            │
run_session(found)                                            │
 │  open command node r/w + extra listen nodes r/o            │
 │  send spec.init (ChatMix enable), then status requests     │
 │  Session::ensure_sinks()  — first sink setup               │
 ▼                                                            │
event loop (poll all fds + `pactl subscribe` stdout, 250 ms)  │
 │                                                            │
 ├─ sink/module added or removed (subscribe event), plus a    │
 │  30 s safety tick — or every 3 s if subscribe is           │
 │  unavailable: ensure_sinks()                               │
 │    snapshot sinks via `pactl -f json list sinks`:          │
 │    • real headset sink gone → unload our modules, wait     │
 │    • our sinks gone / real sink changed → rebuild          │
 │      (covers PipeWire restarts and profile toggles)        │
 │                                                            │
 ├─ every status_poll_secs: re-send status requests           │
 │    (write failure ⇒ device gone → teardown, reconnect) ────┤
 │                                                            │
 └─ fd readable: drain every pending report on every node     │
      (coalesces a fast dial spin into one volume update)     │
      parse_report() → Collected fields                       │
      • GameMix+ChatMix → handle_mix(): set both sink volumes │
      • Power/Battery   → apply_status(): on/off transitions, │
        battery tracking, low-battery notify-send             │
      read error ⇒ device gone → teardown, reconnect ─────────┘

SIGINT/SIGTERM or fatal error → teardown → exit
```

### Session state

`Session` holds the little state the daemon keeps between ticks:

- `sinks` — whether our virtual sinks currently exist, and which real sink
  they loop into.
- `last_mix` — last dial position; replayed after a sink rebuild so volumes
  survive a PipeWire restart.
- `online: Option<bool>` — headset power state, `None` until the first
  status report. Transitions drive default-sink claim/release and logging.
- `prev_default` — the default sink before we claimed it (never one of our
  own sinks), restored on power-off and on exit.
- `battery_pct` / `battery_warned` — low-battery warning with hysteresis:
  warn at ≤ 25 %, re-arm only after recovering to ≥ 50 % or charging.

### PipeWire plumbing

All audio work goes through `pactl` subprocess calls (talking to
pipewire-pulse) — no native PipeWire bindings, which keeps the binary a
trivially static musl build. The operations `Session` needs sit behind the
`Audio` trait (`Pactl` is the real implementation), so the state machine is
unit-tested against a fake with no PipeWire involved (`session::tests`).

- **Change detection**: a `pactl subscribe` child process streams events into
  the session's poll loop; only *new*/*remove* events on sinks and modules
  trigger a re-check ('change' events fire on every volume tweak, including
  our own). If the stream dies (PipeWire restart) or can't start, the daemon
  falls back to 3-second polling and resubscribes once `pactl` works again.

- **Setup** loads, per sink, a `module-null-sink` plus a `module-loopback`
  from its monitor into the real sink (`latency_msec=0`). Before loading,
  any module whose argument string mentions our sink names (including
  pre-rename legacy names) is unloaded, so a crashed previous instance
  can't leave duplicates.
- **Sink naming**: the sink *name* is the display string ("Arctis Game"),
  because Steam Game Mode's picker shows the raw name for card-less sinks
  and ignores `node.nick`/`node.description`. Descriptions are still set
  for desktop tools.
- **Module teardown** parses `pactl list modules short` (tab-separated),
  because the JSON format omits module indices.
- **Default sink**: claimed (set to Arctis Game) when the sinks are built
  and the headset isn't known to be off; released on power-off/exit — but
  only if we still hold it, so a user's manual choice in the meantime is
  never stomped.

### Failure model

Every failure funnels into one of two paths:

- **Transient audio trouble** (pactl fails, sink missing): do nothing this
  tick; the health check (event-driven, with a polling fallback) retries
  forever.
- **Device trouble** (hidraw read/write error): tear down the session
  (release default sink, unload modules) and fall back to the discovery
  loop, which waits for the device to reappear.

This is deliberate: the daemon is dumb about state and re-derives it,
rather than trying to enumerate every way the environment can change.

## Robustness details worth knowing

- `write_command` has a runtime EINVAL fallback that flips between
  numbered/unnumbered report framing and remembers what worked — insurance
  for the hardware-unverified specs whose framing is a guess.
- Frame byte offsets in specs are relative to the hidraw read buffer, which
  differs between numbered and unnumbered interfaces; upstream YAML offsets
  already account for this and are copied verbatim.
- Discovery matches sysfs directly (no libusb/hidapi); the `HID_PHYS` base
  path is used to tie extra listen interfaces to the *same physical device*
  as the command interface.

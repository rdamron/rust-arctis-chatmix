# rust-arctis-chatmix

> [!WARNING]
> **⚠️ Disclaimer: this software was developed with AI** (Claude Code) and
> talks directly to your headset's USB HID interface. Most supported devices
> have **never been tested on real hardware** (see the table below). It is
> provided **as is, with no warranty of any kind**, express or implied — no
> guarantee of merchantability, fitness for a particular purpose, or that it
> won't misbehave with your device. **Use at your own risk.**

ChatMix for SteelSeries **Arctis** headsets on Linux — the thing SteelSeries
Sonar does on Windows, in a single 500 KB static binary with no dependencies.

## Supported devices

| Device | USB product ids (`1038:*`) | ChatMix | Status |
|---|---|---|---|
| Arctis Nova Pro Wireless (+ X) | `12e0` `12e5` `225d` | ✅ | **tested on real hardware** |
| Arctis Nova Pro wired (GameDAC Gen 2) | `12cd` `12cb` | ✅ | untested |
| Arctis Nova 7 / 7X (+ editions) | `2202` `2206` `22a4` `223a` `227a` `22ab` `22a1` `227e` `2258` `229e` `22a9` `22a5` | ✅ | **tested** (Nova 7X Gen 2, `229e`) |
| Arctis 7+ | `220e` `2212` `2216` `2236` | ✅ | untested |
| Arctis Nova Elite | `2244` | ✅ | untested |
| Arctis Nova 5 / 5X | `2232` `2253` `2264` | ⚠️ sinks + battery only¹ | untested |

The untested entries are faithful translations of
[Linux-Arctis-Manager](https://github.com/elegos/Linux-Arctis-Manager)'s device
definitions (see `src/devices.rs`); if you own one, reports either way are very
welcome.

¹ Upstream documents no ChatMix reports for the Nova 5 family, so the daemon
provides the two sinks (adjust their volumes manually) plus battery/status.

## What it does

- Creates two virtual PipeWire sinks, **Arctis Game** (`arctis_game`) and
  **Arctis Chat** (`arctis_chat`), each routed into the headset's real output.
- Sends the device the Sonar/ChatMix enable handshake where it needs one (Nova
  Pro family, Nova Elite), so **pressing the wheel toggles between volume and
  ChatMix mode** on the dock's display.
- Turning the dial in ChatMix mode adjusts the balance: the device reports
  game/chat levels (0–100 each) and the daemon applies them as the two sinks'
  volumes.
- Sets **Arctis Game as the default output** while the headset is on. Move your
  voice app (Discord, etc.) to **Arctis Chat** once in your sound settings —
  PipeWire remembers it from then on.
- **Handles the device coming and going gracefully**: headset powered off
  (default output is handed back to whatever it was before), dock unplugged or
  audio profile disabled (virtual sinks removed, recreated on return), PipeWire
  restarts (sinks rebuilt automatically), suspend/resume, daemon crash
  (leftover sinks are cleaned up on restart).
- Reports headset status on startup and warns in the journal (plus a desktop
  notification) when the battery drops to 25%.

It deliberately does **not** touch settings stored on the device (EQ, mic
volume, sidetone, gain, wireless mode, auto-off) — use the dock's own menu for
those.

## Install

Needs: Linux with systemd + PipeWire (`pactl` available — it is on Bazzite,
Fedora, and virtually everywhere PipeWire is). No root, no distro-specific
anything.

```sh
git clone <this repo> && cd rust-arctis-chatmix
./install.sh
```

That's it. The script:

1. Builds the binary if you have Rust, otherwise uses the prebuilt static
   binary in `dist/` (no toolchain needed).
2. Installs it to `~/.local/bin/rust-arctis-chatmix`.
3. Installs and enables a systemd **user** service, so it starts at every
   login/boot. On Bazzite this includes **Game Mode**: the user session starts
   at auto-login whether you land in gamescope or the desktop, so the daemon
   runs in both.
4. Checks it can access the headset's hidraw device. On Bazzite it can
   already; on stricter distros it installs a udev rule covering every
   supported headset (the only step that asks for sudo, and only when
   needed).

The script detects when it runs inside a distrobox/toolbox and routes
`systemctl` through `distrobox-host-exec` automatically — handy on atomic
distros where the toolchain lives in a container.

Uninstall with `./install.sh --uninstall`.

### Manual install

```sh
cargo build --release --target x86_64-unknown-linux-musl   # or use dist/rust-arctis-chatmix
install -Dm755 target/x86_64-unknown-linux-musl/release/rust-arctis-chatmix ~/.local/bin/rust-arctis-chatmix
install -Dm644 packaging/rust-arctis-chatmix.service ~/.config/systemd/user/rust-arctis-chatmix.service
systemctl --user daemon-reload
systemctl --user enable --now rust-arctis-chatmix

# only if /dev/hidraw* isn't accessible to your user:
sudo install -Dm644 packaging/70-rust-arctis-chatmix.rules /etc/udev/rules.d/70-rust-arctis-chatmix.rules
sudo udevadm control --reload && sudo udevadm trigger
```

## Usage

Once running you shouldn't need to touch it. Daily driving:

- Press the wheel on the dock to switch to ChatMix, turn to balance game vs.
  chat audio.
- Put voice apps on the **Arctis Chat** output (KDE: right-click the app in the
  volume applet → move to Arctis Chat). Everything else stays on the default
  (**Arctis Game**).

Extras:

```sh
rust-arctis-chatmix status      # battery, charge slot, mic, ANC, wireless, ...
journalctl --user -u rust-arctis-chatmix -f    # watch the daemon
```

Flags (edit `ExecStart` in the unit to use them):

- `--no-default-sink` — never touch the default output.
- `--verbose` / `-v` — log every wheel adjustment.

## Troubleshooting

- **Wheel button doesn't switch to ChatMix** — the enable handshake is sent
  when the daemon (re)connects; `systemctl --user restart rust-arctis-chatmix`.
- **No sinks appear** — `journalctl --user -u rust-arctis-chatmix -n 20`. "waiting
  for a supported Arctis device" means nothing was detected on USB; "waiting
  for Arctis audio sink" means PipeWire doesn't show the device's audio output
  (check the card's profile isn't off).
- **Permission denied on /dev/hidraw\*** — rerun `./install.sh`; it installs a
  udev rule for exactly this.

## How it works / credits

Each device speaks a simple HID protocol on a USB interface (4 for the Nova
Pro family, 3 for the rest; 64-byte zero-padded commands). On the Nova Pro
Wireless, for example, `06 8d 01` + `06 49 01` enable Sonar/ChatMix mode,
`06 b0` requests a status report, and wheel turns arrive as
`07 45 <game> <chat>`. The per-device details live in `src/devices.rs`. All of
the protocols were reverse-engineered by the
[Linux-Arctis-Manager](https://github.com/elegos/Linux-Arctis-Manager) project
(see its `devices/*.yaml`) — this tool is a minimal single-binary take on the
same idea. Audio plumbing is plain `pactl` against pipewire-pulse:
`module-null-sink` + `module-loopback`.

## License

[GPL-3.0](LICENSE), matching Linux-Arctis-Manager: the device protocol tables
in `src/devices.rs` are translated from that project's device definitions.

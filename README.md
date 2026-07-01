# arctis-chatmix

ChatMix for the SteelSeries **Arctis Nova Pro Wireless** on Linux — the thing
SteelSeries Sonar does on Windows, in a single 500 KB static binary with no
dependencies.

Supports the Nova Pro Wireless and its X variants (USB ids `1038:12e0`,
`1038:12e5`, `1038:225d`).

## What it does

- Creates two virtual PipeWire sinks, **Arctis Game** (`arctis_game`) and
  **Arctis Chat** (`arctis_chat`), each routed into the headset's real output.
- Sends the base station the Sonar/ChatMix enable handshake, so **pressing the
  wheel toggles between volume and ChatMix mode** on the dock's display.
- Turning the wheel in ChatMix mode adjusts the balance: the dock reports
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
git clone <this repo> && cd arctis-chatmix
./install.sh
```

That's it. The script:

1. Builds the binary if you have Rust, otherwise uses the prebuilt static
   binary in `dist/` (no toolchain needed).
2. Installs it to `~/.local/bin/arctis-chatmix`.
3. Installs and enables a systemd **user** service, so it starts at every
   login/boot. On Bazzite this includes **Game Mode**: the user session starts
   at auto-login whether you land in gamescope or the desktop, so the daemon
   runs in both.
4. Checks it can access the dock's hidraw device. On Bazzite it can already;
   on stricter distros it offers to install a one-line udev rule (the only
   step that asks for sudo, and only when needed).

The script detects when it runs inside a distrobox/toolbox and routes
`systemctl` through `distrobox-host-exec` automatically — handy on atomic
distros where the toolchain lives in a container.

Uninstall with `./install.sh --uninstall`.

### Manual install

```sh
cargo build --release --target x86_64-unknown-linux-musl   # or use dist/arctis-chatmix
install -Dm755 target/x86_64-unknown-linux-musl/release/arctis-chatmix ~/.local/bin/arctis-chatmix
install -Dm644 packaging/arctis-chatmix.service ~/.config/systemd/user/arctis-chatmix.service
systemctl --user daemon-reload
systemctl --user enable --now arctis-chatmix
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
arctis-chatmix status      # battery, charge slot, mic, ANC, wireless, ...
journalctl --user -u arctis-chatmix -f    # watch the daemon
```

Flags (edit `ExecStart` in the unit to use them):

- `--no-default-sink` — never touch the default output.
- `--verbose` / `-v` — log every wheel adjustment.

## Troubleshooting

- **Wheel button doesn't switch to ChatMix** — the enable handshake is sent
  when the daemon (re)connects; `systemctl --user restart arctis-chatmix`.
- **No sinks appear** — `journalctl --user -u arctis-chatmix -n 20`. "waiting
  for base station" means the dock isn't detected on USB; "waiting for Arctis
  audio sink" means PipeWire doesn't show the dock's audio device (check the
  card's profile isn't off).
- **Permission denied on /dev/hidraw\*** — rerun `./install.sh`; it installs a
  udev rule for exactly this.

## How it works / credits

The dock speaks a simple HID protocol on USB interface 4 (64-byte zero-padded
commands): `06 8d 01` + `06 49 01` enable Sonar/ChatMix mode, `06 b0` requests
a status report, and wheel turns arrive as `07 45 <game> <chat>`. All of it was
reverse-engineered by the
[Linux-Arctis-Manager](https://github.com/elegos/Linux-Arctis-Manager) project
(see `devices/nova_pro_wireless.yaml` there) — this tool is a minimal
single-binary take on the same idea. Audio plumbing is plain `pactl` against
pipewire-pulse: `module-null-sink` + `module-loopback`.

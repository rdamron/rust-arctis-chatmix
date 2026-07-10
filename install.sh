#!/usr/bin/env bash
# Installer for rust-arctis-chatmix. Everything is user-level (~/.local/bin +
# systemd user unit); the only optional root step is a udev rule on distros
# where hidraw isn't accessible to the seated user (not needed on Bazzite).
#
# Works from inside a distrobox/toolbox too: systemctl calls fall back to
# distrobox-host-exec / flatpak-spawn when the host systemd isn't reachable.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN=rust-arctis-chatmix
TARGET=x86_64-unknown-linux-musl
BIN_DIR="$HOME/.local/bin"
UNIT_DIR="$HOME/.config/systemd/user"
UDEV_RULE=/etc/udev/rules.d/70-rust-arctis-chatmix.rules

say() { printf '\033[1m==>\033[0m %s\n' "$*"; }

run_systemctl() {
    if command -v systemctl >/dev/null 2>&1 && systemctl --user show-environment >/dev/null 2>&1; then
        systemctl --user "$@"
    elif command -v distrobox-host-exec >/dev/null 2>&1; then
        distrobox-host-exec systemctl --user "$@"
    elif command -v flatpak-spawn >/dev/null 2>&1; then
        flatpak-spawn --host systemctl --user "$@"
    else
        echo "warning: couldn't reach systemd; run manually: systemctl --user $*" >&2
        return 1
    fi
}

uninstall() {
    say "Stopping and disabling service"
    run_systemctl disable --now rust-arctis-chatmix 2>/dev/null || true
    rm -f "$BIN_DIR/$BIN" "$UNIT_DIR/rust-arctis-chatmix.service"
    run_systemctl daemon-reload || true
    say "Removed $BIN_DIR/$BIN and the systemd unit."
    if [ -e "$UDEV_RULE" ]; then
        say "udev rule left in place; remove with: sudo rm $UDEV_RULE"
    fi
    exit 0
}

case "${1:-}" in
    --uninstall) uninstall ;;
    "") ;;
    *) echo "usage: $0 [--uninstall]" >&2; exit 2 ;;
esac

# --- 1. get a binary: build if a toolchain is around, else download the
#        latest release (built by CI from the tagged source)
RELEASE_URL="https://github.com/rdamron/rust-arctis-chatmix/releases/latest/download/$BIN"
SRC=""
if command -v cargo >/dev/null 2>&1; then
    if rustup target list --installed 2>/dev/null | grep -q "$TARGET"; then
        say "Building static binary ($TARGET)"
        (cd "$SCRIPT_DIR" && cargo build --release --target "$TARGET")
        SRC="$SCRIPT_DIR/target/$TARGET/release/$BIN"
    else
        say "Building binary (add the $TARGET rustup target for a static build)"
        (cd "$SCRIPT_DIR" && cargo build --release)
        SRC="$SCRIPT_DIR/target/release/$BIN"
    fi
else
    say "No Rust toolchain found; downloading the latest release binary"
    SRC="$(mktemp -t rust-arctis-chatmix.XXXXXX)"
    trap 'rm -f "$SRC"' EXIT
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL -o "$SRC" "$RELEASE_URL"
    elif command -v wget >/dev/null 2>&1; then
        wget -qO "$SRC" "$RELEASE_URL"
    else
        echo "error: need cargo (https://rustup.rs), or curl/wget to fetch $RELEASE_URL" >&2
        exit 1
    fi
    if [ "$(head -c 4 "$SRC")" != $'\x7fELF' ]; then
        echo "error: download from $RELEASE_URL doesn't look like a binary" >&2
        exit 1
    fi
fi

say "Installing $BIN to $BIN_DIR"
install -Dm755 "$SRC" "$BIN_DIR/$BIN"
install -Dm644 "$SCRIPT_DIR/packaging/rust-arctis-chatmix.service" "$UNIT_DIR/rust-arctis-chatmix.service"

say "Enabling and (re)starting the systemd user service"
run_systemctl daemon-reload
run_systemctl enable rust-arctis-chatmix
run_systemctl restart rust-arctis-chatmix

# --- 2. hidraw permission check (root only needed if this fails)
# Supported product ids come from the shipped udev rules file, which a cargo
# test keeps in sync with src/devices.rs.
RULES_SRC="$SCRIPT_DIR/packaging/70-rust-arctis-chatmix.rules"
ids="$(sed -n 's/.*idProduct}=="\([0-9a-f]*\)".*/\1/p' "$RULES_SRC" | tr 'a-f' 'A-F' | paste -sd'|')"
found=""
denied=""
for h in /sys/class/hidraw/hidraw*; do
    [ -e "$h" ] || continue
    grep -qE "HID_ID=0003:00001038:0000($ids)\$" "$h/device/uevent" 2>/dev/null || continue
    node="/dev/$(basename "$h")"
    found="$node"
    { [ -r "$node" ] && [ -w "$node" ]; } || denied="$node"
done
if [ -z "$found" ]; then
    say "No supported Arctis device detected right now; skipping the permission check."
    say "(The daemon waits for the device, so this is fine.)"
elif [ -z "$denied" ]; then
    say "hidraw permissions OK ($found)"
else
    say "No access to $denied — installing a udev rule (needs sudo)"
    sudo install -Dm644 "$RULES_SRC" "$UDEV_RULE"
    sudo udevadm control --reload
    sudo udevadm trigger
fi

say "Done."
say "Logs:            journalctl --user -u rust-arctis-chatmix -f"
say "Headset status:  $BIN status"

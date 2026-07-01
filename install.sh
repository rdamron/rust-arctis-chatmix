#!/usr/bin/env bash
# Installer for arctis-chatmix. Everything is user-level (~/.local/bin +
# systemd user unit); the only optional root step is a udev rule on distros
# where hidraw isn't accessible to the seated user (not needed on Bazzite).
#
# Works from inside a distrobox/toolbox too: systemctl calls fall back to
# distrobox-host-exec / flatpak-spawn when the host systemd isn't reachable.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN=arctis-chatmix
TARGET=x86_64-unknown-linux-musl
BIN_DIR="$HOME/.local/bin"
UNIT_DIR="$HOME/.config/systemd/user"
UDEV_RULE=/etc/udev/rules.d/70-arctis-chatmix.rules

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
    run_systemctl disable --now arctis-chatmix 2>/dev/null || true
    rm -f "$BIN_DIR/$BIN" "$UNIT_DIR/arctis-chatmix.service"
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

# --- 1. get a binary: build if a toolchain is around, else use the prebuilt one
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
elif [ -x "$SCRIPT_DIR/dist/$BIN" ]; then
    say "No Rust toolchain found; using prebuilt static binary from dist/"
    SRC="$SCRIPT_DIR/dist/$BIN"
else
    echo "error: need either cargo (https://rustup.rs) or the prebuilt dist/$BIN" >&2
    exit 1
fi

say "Installing $BIN to $BIN_DIR"
install -Dm755 "$SRC" "$BIN_DIR/$BIN"
install -Dm644 "$SCRIPT_DIR/packaging/arctis-chatmix.service" "$UNIT_DIR/arctis-chatmix.service"

say "Enabling and (re)starting the systemd user service"
run_systemctl daemon-reload
run_systemctl enable arctis-chatmix
run_systemctl restart arctis-chatmix

# --- 2. hidraw permission check (root only needed if this fails)
node=""
for h in /sys/class/hidraw/hidraw*; do
    [ -e "$h" ] || continue
    # match the command interface (USB interface 4), the node the daemon opens
    if grep -qE 'HID_ID=0003:00001038:0000(12E0|12E5|225D)' "$h/device/uevent" 2>/dev/null &&
       grep -q 'HID_PHYS=.*/input4$' "$h/device/uevent" 2>/dev/null; then
        node="/dev/$(basename "$h")"
        break
    fi
done
if [ -z "$node" ]; then
    say "Base station not detected right now; skipping the permission check."
    say "(The daemon waits for the device, so this is fine.)"
elif [ -r "$node" ] && [ -w "$node" ]; then
    say "hidraw permissions OK ($node)"
else
    say "No access to $node — installing a udev rule (needs sudo)"
    sudo tee "$UDEV_RULE" >/dev/null <<'EOF'
# SteelSeries Arctis Nova Pro Wireless base station — user access for arctis-chatmix
KERNEL=="hidraw*", ATTRS{idVendor}=="1038", ATTRS{idProduct}=="12e0", TAG+="uaccess"
KERNEL=="hidraw*", ATTRS{idVendor}=="1038", ATTRS{idProduct}=="12e5", TAG+="uaccess"
KERNEL=="hidraw*", ATTRS{idVendor}=="1038", ATTRS{idProduct}=="225d", TAG+="uaccess"
EOF
    sudo udevadm control --reload
    sudo udevadm trigger
fi

say "Done."
say "Logs:            journalctl --user -u arctis-chatmix -f"
say "Headset status:  $BIN status"

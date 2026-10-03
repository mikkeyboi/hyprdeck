#!/usr/bin/env bash
# Build and install hyprdeck for the current user.
#   ./install.sh           build + install (restarts the running service)
#   ./install.sh --enable  also enable launch at login
set -euo pipefail
cd "$(dirname "$0")"

cargo build --release
install -Dm755 target/release/hyprdeck "$HOME/.local/bin/hyprdeck"
install -Dm644 data/io.github.mikkeyboi.Hyprdeck.desktop "$HOME/.local/share/applications/io.github.mikkeyboi.Hyprdeck.desktop"
install -Dm644 data/icons/io.github.mikkeyboi.Hyprdeck.svg "$HOME/.local/share/icons/hicolor/scalable/apps/io.github.mikkeyboi.Hyprdeck.svg"
install -Dm644 data/hyprdeck.service "$HOME/.config/systemd/user/hyprdeck.service"
gtk-update-icon-cache -q -t "$HOME/.local/share/icons/hicolor" 2>/dev/null || true
systemctl --user daemon-reload

if [[ "${1:-}" == "--enable" ]]; then
    systemctl --user enable hyprdeck.service
fi
if systemctl --user is-active -q hyprdeck.service; then
    systemctl --user restart hyprdeck.service
    echo "hyprdeck installed and restarted."
else
    echo "hyprdeck installed. Start now: systemctl --user start hyprdeck.service"
fi

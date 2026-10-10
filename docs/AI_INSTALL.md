# Installing Hyprdeck — instructions for AI assistants

This file is written for an AI coding assistant (Claude, Codex, Copilot, …) helping a user install
Hyprdeck and run it as a login service. Follow the steps in order, run the checks, and report
results to the user. Ask the user before anything that needs `sudo` or changes their Hyprland config.

## 0. Ground rules

- Never run `sudo` without the user's explicit approval for that exact command.
- Back up before changing config: `cp -a ~/.config/hypr ~/.config/hypr.bak-$(date +%Y%m%d%H%M%S)`.
- Hyprdeck only adds one line to `~/.config/hypr/hyprland.lua` (`require("hyprdeck")`, appended the first
  time a setting is applied) and writes its own files listed in step 6. Don't hand-edit
  `~/.config/hypr/hyprdeck.lua`: it is regenerated from `~/.config/hyprdeck/hyprland.toml`.

## 1. Check the environment

Run these and interpret them:

```sh
hyprctl version | head -1                      # need Hyprland 0.56+ (Lua config)
ls ~/.config/hypr/hyprland.lua                 # Lua config entry point must exist
ls /usr/share/hypr/stubs/hl.meta.lua           # Lua API stubs (shipped with Hyprland 0.56+)
systemctl --user is-active graphical-session.target   # "active" → systemd-managed session (uwsm etc.)
command -v pacman paru yay checkupdates pactl bluetoothctl ddcutil
echo "$XDG_CURRENT_DESKTOP $XDG_SESSION_TYPE"  # expect "Hyprland wayland"
```

- If there is only `hyprland.conf` (no `hyprland.lua`), the display, input and keybind pages can still
  read live state, but they apply settings through a generated Lua file, so they will have no effect.
  Tell the user, and don't convert their config yourself unless they ask you to.
- Optional features turn themselves off when their tool is missing. Tell the user which ones apply:
  - `pacman`/`checkupdates`/`paru`/`yay`: the Updates page. It needs Arch or an Arch-based distro and
    `pacman-contrib`.
  - `pactl` from `pipewire-pulse`: the Audio page.
  - `bluetoothd`: the Bluetooth page.
  - `ddcutil`: monitor brightness/contrast. The user also needs to be in the `i2c` group and have the
    `i2c-dev` module loaded.

## 2a. Install from the AppImage (recommended)

```sh
mkdir -p ~/.local/opt/hyprdeck ~/.local/bin
cd ~/.local/opt/hyprdeck
curl -fLO https://github.com/mikkeyboi/hyprdeck/releases/latest/download/Hyprdeck-x86_64.AppImage
curl -fLO https://github.com/mikkeyboi/hyprdeck/releases/latest/download/Hyprdeck-x86_64.AppImage.sha256
sha256sum -c Hyprdeck-x86_64.AppImage.sha256
chmod +x Hyprdeck-x86_64.AppImage
ln -sf ~/.local/opt/hyprdeck/Hyprdeck-x86_64.AppImage ~/.local/bin/hyprdeck
hyprdeck --version
```

The AppImage bundles GTK4/libadwaita, but it is built on Arch Linux. On distros with an older glibc it may
not start. If so, use 2b. If it fails with a FUSE error, install `fuse2` (ask first: it needs sudo), or run it
with `APPIMAGE_EXTRACT_AND_RUN=1`.

Desktop entry and icon (so app launchers can show it):

```sh
cd /tmp && ~/.local/opt/hyprdeck/Hyprdeck-x86_64.AppImage --appimage-extract >/dev/null
install -Dm644 squashfs-root/usr/share/applications/io.github.mikkeyboi.Hyprdeck.desktop \
  ~/.local/share/applications/io.github.mikkeyboi.Hyprdeck.desktop
install -Dm644 squashfs-root/usr/share/icons/hicolor/scalable/apps/io.github.mikkeyboi.Hyprdeck.svg \
  ~/.local/share/icons/hicolor/scalable/apps/io.github.mikkeyboi.Hyprdeck.svg
rm -rf squashfs-root
```

The desktop file runs `hyprdeck`, which resolves to the `~/.local/bin` symlink. Check that `~/.local/bin`
is on the session's `PATH` with `systemctl --user show-environment | grep ^PATH=`.

## 2b. Install from source

Needs Rust 1.88+, GTK ≥ 4.20 and libadwaita ≥ 1.8 development files. On Arch:
`sudo pacman -S --needed rust gtk4 libadwaita base-devel` (ask first).

```sh
git clone https://github.com/mikkeyboi/hyprdeck ~/.local/src/hyprdeck
cd ~/.local/src/hyprdeck
./install.sh            # builds release, installs binary, desktop file, icon and user unit
```

## 3. Run it as a login service

This unit is what `install.sh` installs. For an AppImage install, write it yourself:

```ini
# ~/.config/systemd/user/hyprdeck.service
[Unit]
Description=Hyprdeck desktop control center (tray, audio routing, wake guard, update checks)
PartOf=graphical-session.target
After=graphical-session.target
Wants=pipewire.service pipewire-pulse.service
After=pipewire.service pipewire-pulse.service

[Service]
Type=exec
ExecStart=%h/.local/bin/hyprdeck --background
Restart=on-failure
RestartSec=3
TimeoutStopSec=10
Slice=app-graphical.slice

[Install]
WantedBy=graphical-session.target
```

```sh
systemctl --user daemon-reload
systemctl --user enable --now hyprdeck.service
```

If `graphical-session.target` is **not** active (Hyprland started without uwsm or another systemd
session manager), the unit will never start. In that case, don't enable the unit. Ask the user, then add
this to the `hl.on("hyprland.start", function() … end)` block in their Hyprland config:

```lua
hl.exec_cmd("hyprdeck --background")
```

`--background` starts only the tray and background services. The user can change this under
Preferences → "Start minimized" (on by default).

## 4. Verify

```sh
systemctl --user status hyprdeck.service --no-pager | head -5   # active (running)
busctl --user list | grep -i hyprdeck                           # io.github.mikkeyboi.Hyprdeck + a StatusNotifierItem
hyprdeck --page display                                         # opens the window on the Displays page
journalctl --user -u hyprdeck -n 50 --no-pager                  # no errors
```

Ask the user to confirm that the tray icon is visible. This needs a StatusNotifierItem host such as
Noctalia, Waybar's tray or ironbar. Without a tray host, Hyprdeck opens its window instead, and closing
the window quits it.

## 5. Things to tell the user after installing

- **HyprMod users:** the first time a setting is applied, Hyprdeck imports `hyprland-gui.lua` into its own
  settings, moves the file to `~/.config/hyprdeck/backup/`, and replaces `require("hyprland-gui")` with
  `require("hyprdeck")`. Suggest uninstalling HyprMod afterwards so it doesn't write the file again.
- **Audio:** simultaneous output uses a PipeWire combine sink named `simultaneous_output`. If another tool
  already makes combine sinks, turn one of them off.
- **Sleep & Wake:** the resume guard is on by default. After each wake it writes diagnostics to
  `~/.local/state/hyprdeck/resume/`.
- **Plugins:** peripheral backends are separate releases and are not bundled. See
  [PLUGINS.md](PLUGINS.md) for build/install instructions. Installing a plugin does not execute it;
  enabling explicitly trusts unsandboxed code running as the user. Never enable an unrelated plugin
  or apply its update without the user's authorization. Plugin updates do not flash hardware.

## 6. Files Hyprdeck owns

| Path | Purpose |
| --- | --- |
| `~/.config/hyprdeck/*.toml` | Feature settings, including `plugins.toml` enabled/trusted plugin IDs |
| `~/.config/hyprdeck/backup/` | Imported HyprMod file |
| `~/.config/hypr/hyprdeck.lua` | Generated Hyprland settings, `require`d last by `hyprland.lua` |
| `~/.local/state/hyprdeck/` | Resume diagnostics, update report, GitHub release cache |
| `~/.local/share/hyprdeck/plugins/` | External plugin manifests and executables (or under `$XDG_DATA_HOME`) |
| `~/.config/systemd/user/hyprdeck.service` | Login service |

## 7. Uninstall

```sh
systemctl --user disable --now hyprdeck.service
rm -f ~/.config/systemd/user/hyprdeck.service ~/.local/bin/hyprdeck
rm -rf ~/.local/opt/hyprdeck
rm -f ~/.local/share/applications/io.github.mikkeyboi.Hyprdeck.desktop \
      ~/.local/share/icons/hicolor/scalable/apps/io.github.mikkeyboi.Hyprdeck.svg
systemctl --user daemon-reload
```

Then remove the `-- hyprdeck managed settings (keep last)` / `require("hyprdeck")` lines from
`~/.config/hypr/hyprland.lua` and delete `~/.config/hypr/hyprdeck.lua`. Run `hyprctl reload`. Settings in
`~/.config/hyprdeck/` can be deleted, or kept for a reinstall.

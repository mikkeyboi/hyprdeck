#!/usr/bin/env bash
# Build Hyprdeck-x86_64.AppImage (+ .sha256) from the workspace.
#
#   packaging/appimage/build-appimage.sh [--skip-build]
#
# Needs: cargo, curl, and the GTK4/libadwaita runtime libraries the binary links
# against (they get bundled). Works without FUSE (containers/CI) via
# APPIMAGE_EXTRACT_AND_RUN. Output lands in ./dist/.
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root"

app_id="io.github.mikkeyboi.Hyprdeck"
version="$(cargo metadata --no-deps --format-version 1 | sed -n 's/.*"name":"hyprdeck","version":"\([^"]*\)".*/\1/p')"
tools="${HYPRDECK_APPIMAGE_TOOLS:-$root/target/appimage-tools}"
appdir="$root/target/AppDir"
dist="$root/dist"

if [[ "${1:-}" != "--skip-build" ]]; then
    cargo build --release --locked
fi

mkdir -p "$tools" "$dist"
fetch() { # url dest
    [[ -s "$2" ]] || curl -fsSL --retry 3 -o "$2" "$1"
    chmod +x "$2"
}
fetch https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage \
    "$tools/linuxdeploy-x86_64.AppImage"
# Pinned plugin revision. Distros whose GTK4 ships no module dir (e.g. Arch has
# no /usr/lib/gtk-4.0) make its unconditional copy fail, so skip that copy then.
gtk_plugin_rev=7a3fbc31a9e5075073ff8790f26effbac5f84453
if [[ ! -s "$tools/linuxdeploy-plugin-gtk.sh" ]]; then
    fetch "https://raw.githubusercontent.com/linuxdeploy/linuxdeploy-plugin-gtk/$gtk_plugin_rev/linuxdeploy-plugin-gtk.sh" \
        "$tools/linuxdeploy-plugin-gtk.sh"
    sed -i 's|^\( *\)copy_lib_tree "\$gtk4_libdir" "\$APPDIR/"$|\1[ ! -d "$gtk4_libdir" ] \|\| copy_lib_tree "$gtk4_libdir" "$APPDIR/"|' \
        "$tools/linuxdeploy-plugin-gtk.sh"
    grep -q '\[ ! -d "\$gtk4_libdir" \]' "$tools/linuxdeploy-plugin-gtk.sh" \
        || { echo "linuxdeploy-plugin-gtk patch did not apply" >&2; exit 1; }
fi

rm -rf "$appdir"
install -Dm755 target/release/hyprdeck "$appdir/usr/bin/hyprdeck"
install -Dm644 "data/$app_id.desktop" "$appdir/usr/share/applications/$app_id.desktop"
install -Dm644 "data/icons/$app_id.svg" "$appdir/usr/share/icons/hicolor/scalable/apps/$app_id.svg"
install -Dm644 "data/$app_id.metainfo.xml" "$appdir/usr/share/metainfo/$app_id.metainfo.xml"
# Runs after the gtk plugin's hook (hooks are sourced in name order). The plugin
# forces X11, GTK_THEME=Adwaita and an AppDir-only GTK data prefix, which break
# libadwaita's stylesheet and the user's ~/.config/gtk-4.0 theme. Hyprdeck is a
# native Wayland app: keep only the bundled libraries, schemas and loaders.
install -Dm644 /dev/stdin "$appdir/apprun-hooks/zz-hyprdeck.sh" <<'EOF'
unset GDK_BACKEND GTK_DATA_PREFIX GTK_EXE_PREFIX GTK_PATH
if [ -z "${APPIMAGE_GTK_THEME:-}" ]; then
    unset GTK_THEME
fi
EOF

export APPIMAGE_EXTRACT_AND_RUN=1
export DEPLOY_GTK_VERSION=4
export VERSION="$version"
export LDAI_OUTPUT="$dist/Hyprdeck-x86_64.AppImage"
export PATH="$tools:$PATH"
rm -f "$LDAI_OUTPUT"

"$tools/linuxdeploy-x86_64.AppImage" \
    --appdir "$appdir" \
    --executable "$appdir/usr/bin/hyprdeck" \
    --desktop-file "$appdir/usr/share/applications/$app_id.desktop" \
    --icon-file "$appdir/usr/share/icons/hicolor/scalable/apps/$app_id.svg" \
    --plugin gtk \
    --output appimage

(cd "$dist" && sha256sum Hyprdeck-x86_64.AppImage > Hyprdeck-x86_64.AppImage.sha256)
echo "Built $dist/Hyprdeck-x86_64.AppImage ($version)"

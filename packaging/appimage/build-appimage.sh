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
# Same rule as crates/core/build.rs: release CI sets HYPRDECK_VERSION, otherwise
# the nearest vX.Y.Z tag, otherwise the crate version.
version="${HYPRDECK_VERSION:-$(git describe --tags --match 'v[0-9]*' --abbrev=0 2>/dev/null | sed 's/^v//')}"
version="${version:-$(cargo metadata --no-deps --format-version 1 | sed -n 's/.*"name":"hyprdeck","version":"\([^"]*\)".*/\1/p')}"
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

rm -rf "$appdir"
install -Dm755 target/release/hyprdeck "$appdir/usr/bin/hyprdeck"
install -Dm644 "data/$app_id.desktop" "$appdir/usr/share/applications/$app_id.desktop"
install -Dm644 "data/icons/$app_id.svg" "$appdir/usr/share/icons/hicolor/scalable/apps/$app_id.svg"
install -Dm644 "data/$app_id.metainfo.xml" "$appdir/usr/share/metainfo/$app_id.metainfo.xml"
sed -i "s|<release version=\"[^\"]*\" date=\"[^\"]*\"/>|<release version=\"$version\" date=\"$(date -u +%Y-%m-%d)\"/>|" \
    "$appdir/usr/share/metainfo/$app_id.metainfo.xml"

# GTK4 needs its own GSettings schemas (file chooser, settings); bundle them so
# the AppImage works on hosts without GTK4. Image loading uses the host's
# gdk-pixbuf/glycin loaders, and themes come from the host, so no GTK/pixbuf
# environment overrides (linuxdeploy-plugin-gtk's X11/GTK_THEME forcing breaks
# libadwaita and native Wayland).
schemas="$appdir/usr/share/glib-2.0/schemas"
install -d "$schemas"
cp /usr/share/glib-2.0/schemas/org.gtk.gtk4.*.xml "$schemas/"
glib-compile-schemas "$schemas"
install -Dm644 /dev/stdin "$appdir/apprun-hooks/hyprdeck.sh" <<'EOF'
# Bundled data (GTK4 schemas, icon) first; host data dirs keep themes and apps visible.
export XDG_DATA_DIRS="$APPDIR/usr/share:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"
EOF

export APPIMAGE_EXTRACT_AND_RUN=1
export VERSION="$version"
export LDAI_OUTPUT="$dist/Hyprdeck-x86_64.AppImage"
rm -f "$LDAI_OUTPUT"

"$tools/linuxdeploy-x86_64.AppImage" \
    --appdir "$appdir" \
    --executable "$appdir/usr/bin/hyprdeck" \
    --desktop-file "$appdir/usr/share/applications/$app_id.desktop" \
    --icon-file "$appdir/usr/share/icons/hicolor/scalable/apps/$app_id.svg" \
    --output appimage

(cd "$dist" && sha256sum Hyprdeck-x86_64.AppImage > Hyprdeck-x86_64.AppImage.sha256)
echo "Built $dist/Hyprdeck-x86_64.AppImage ($version)"

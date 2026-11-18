#!/usr/bin/env bash
# Installs the built t3boat AppImage for the current user on Omarchy (or any Linux desktop):
# the app in ~/Applications/t3boat, its icon, and an app launcher entry (Walker, etc.).
#
#   bun install && CARGO_BUILD_JOBS=4 bun run build:desktop
#   scripts/install-omarchy.sh
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
tauri="$repo/apps/desktop/src-tauri"
bundle="$tauri/target/release/bundle/appimage"

appimage="$(ls -t "$bundle"/t3boat_*.AppImage 2>/dev/null | head -n 1 || true)"
if [ -z "$appimage" ]; then
  echo "No AppImage in $bundle. Build it first: bun run build:desktop" >&2
  exit 1
fi

app_dir="$HOME/Applications/t3boat"
target="$app_dir/t3boat.AppImage"
icon_dir="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/128x128/apps"
apps_dir="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
mkdir -p "$app_dir" "$icon_dir" "$apps_dir"

# A running t3boat keeps the old file open; replace it rather than writing into it.
install -m 755 "$appimage" "$target.new"
mv -f "$target.new" "$target"
install -m 644 "$tauri/icons/128x128.png" "$icon_dir/t3boat.png"

cat >"$apps_dir/t3boat.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=t3boat
Comment=Cloud machines for T3 Code agents, on Boat
Exec=$target
Icon=t3boat
Terminal=false
Categories=Development;
StartupWMClass=t3boat
EOF

command -v update-desktop-database >/dev/null && update-desktop-database "$apps_dir" >/dev/null 2>&1 || true
echo "Installed $target"
echo "Open it from your app launcher, or run: $target"

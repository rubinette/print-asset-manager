#!/bin/sh
# Installs 列印素材庫 for the current user. Pass a prefix to install elsewhere,
# e.g. `sudo ./install.sh /usr/local`.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
prefix=${1:-"$HOME/.local"}

install -Dm755 "$here/print-asset-manager" "$prefix/bin/print-asset-manager"
mkdir -p "$prefix/share/applications"
sed "s|^Exec=.*|Exec=$prefix/bin/print-asset-manager|" "$here/print-asset-manager.desktop" \
  > "$prefix/share/applications/print-asset-manager.desktop"
install -Dm644 "$here/print-asset-manager.png" "$prefix/share/pixmaps/print-asset-manager.png"
command -v update-desktop-database >/dev/null && update-desktop-database "$prefix/share/applications" || true

echo "已安裝到 $prefix/bin/print-asset-manager"

#!/bin/sh
# Packs a built binary with its desktop entry, icon and installer into a
# tarball.
#
#   packaging/linux/package.sh <binary> <version> <arch> <out-dir>
set -eu
bin=$1 version=$2 arch=$3 out=$4
here=$(cd "$(dirname "$0")" && pwd)
name="print-asset-manager-$version-linux-$arch"
dir="$out/$name"

rm -rf "$dir"
mkdir -p "$dir"
install -m755 "$bin" "$dir/print-asset-manager"
install -m644 "$here/print-asset-manager.desktop" "$dir/"
install -m644 "$here/../icon.png" "$dir/print-asset-manager.png"
install -m755 "$here/install.sh" "$dir/"
tar -C "$out" -czf "$out/$name.tar.gz" "$name"

#!/bin/sh
# Wraps a built binary into 列印素材庫.app and a .dmg. macOS only (sips,
# iconutil, hdiutil). The app gets an ad-hoc signature so Apple Silicon will
# run it; it is not notarized, so Gatekeeper asks before the first launch.
#
#   packaging/macos/bundle.sh <binary> <version> <out-dir>
set -eu
bin=$1 version=$2 out=$3
here=$(cd "$(dirname "$0")" && pwd)
app="$out/列印素材庫.app"

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin" "$app/Contents/MacOS/print-asset-manager"
sed "s/__VERSION__/$version/g" "$here/Info.plist" > "$app/Contents/Info.plist"

iconset=$(mktemp -d)/AppIcon.iconset
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
  sips -z $size $size "$here/../icon.png" --out "$iconset/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z $double $double "$here/../icon.png" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/AppIcon.icns"

codesign --force --deep --sign - "$app"

staging=$(mktemp -d)
cp -R "$app" "$staging/"
ln -s /Applications "$staging/Applications"
hdiutil create -volname "列印素材庫" -srcfolder "$staging" -fs HFS+ -format UDZO -ov \
  "$out/print-asset-manager-$version-macos-universal.dmg"

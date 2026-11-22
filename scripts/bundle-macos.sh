#!/bin/sh
# Build the desktop app and assemble dist/AIOM.app.
#
#   scripts/bundle-macos.sh             build dist/AIOM.app
#   scripts/bundle-macos.sh --install   also copy it to /Applications
#                                       (or ~/Applications if that is not writable)
set -eu

cd "$(dirname "$0")/.."
PATH="$HOME/.cargo/bin:$PATH"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' app/Cargo.toml | head -1)
APP="dist/AIOM.app"

cargo build --release -p aiom-app

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/aiom-app "$APP/Contents/MacOS/AIOM"

# App icon: every size macOS asks for, from the one 1024px PNG.
WORK=$(mktemp -d)
ICONSET="$WORK/AIOM.iconset"
mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
  double=$((size * 2))
  sips -z "$size" "$size" app/icons/icon.png --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
  sips -z "$double" "$double" app/icons/icon.png --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AIOM.icns"
rm -rf "$WORK"

cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>AIOM</string>
  <key>CFBundleDisplayName</key><string>AIOM</string>
  <key>CFBundleIdentifier</key><string>dev.aiom.app</string>
  <key>CFBundleExecutable</key><string>AIOM</string>
  <key>CFBundleIconFile</key><string>AIOM</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>CFBundleDevelopmentRegion</key><string>zh_TW</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSAppleEventsUsageDescription</key><string>AIOM 切換 Claude 桌面 App 的帳號時，需要先關閉 Claude，完成後再重新開啟。</string>
</dict>
</plist>
EOF

# Ad-hoc signature so macOS treats the bundle as one consistent app.
codesign --force --sign - "$APP" >/dev/null 2>&1 || echo "note: codesign failed; the app still runs locally"

echo "built $APP"

if [ "${1:-}" = "--install" ]; then
  if [ -w /Applications ]; then
    TARGET=/Applications
  else
    TARGET="$HOME/Applications"
    mkdir -p "$TARGET"
  fi
  rm -rf "$TARGET/AIOM.app"
  cp -R "$APP" "$TARGET/AIOM.app"
  echo "installed $TARGET/AIOM.app"
fi

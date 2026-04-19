#!/bin/bash
# Build a macOS .app bundle for mlink. The bundle's Info.plist declares the
# Bluetooth usage strings macOS requires to grant Core Bluetooth access to a
# non-sandboxed binary; launching mlink via the bundle (vs. running the raw
# binary from Terminal) is what triggers the permission prompt.
set -e

APP_NAME="mlink"
APP_DIR="target/release/${APP_NAME}.app"
CONTENTS="${APP_DIR}/Contents"
MACOS="${CONTENTS}/MacOS"
RESOURCES="${CONTENTS}/Resources"

cargo build --release

rm -rf "${APP_DIR}"
mkdir -p "${MACOS}" "${RESOURCES}"

cp "target/release/${APP_NAME}" "${MACOS}/${APP_NAME}"

cat > "${CONTENTS}/Info.plist" << 'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>mlink</string>
    <key>CFBundleDisplayName</key>
    <string>mlink</string>
    <key>CFBundleIdentifier</key>
    <string>com.mlink.app</string>
    <key>CFBundleVersion</key>
    <string>0.1.0</string>
    <key>CFBundleShortVersionString</key>
    <string>0.1.0</string>
    <key>CFBundleExecutable</key>
    <string>mlink</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>LSMinimumSystemVersion</key>
    <string>12.0</string>
    <key>NSBluetoothAlwaysUsageDescription</key>
    <string>mlink needs Bluetooth to discover and connect nearby devices.</string>
    <key>NSBluetoothPeripheralUsageDescription</key>
    <string>mlink needs Bluetooth to advertise as a connectable device.</string>
    <key>LSUIElement</key>
    <true/>
</dict>
</plist>
PLIST

echo "Bundle created: ${APP_DIR}"

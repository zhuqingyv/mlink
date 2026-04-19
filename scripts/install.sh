#!/bin/bash
# Install mlink as a .app in /Applications and symlink the CLI into
# /usr/local/bin so `mlink` is usable from any terminal. Bluetooth permission
# is bound to the .app's CFBundleIdentifier, so running the symlinked binary
# inherits the bundle's permission grant.
set -e

bash scripts/bundle-macos.sh

# /Applications often requires root, but macOS will prompt if needed.
cp -R target/release/mlink.app /Applications/

# /usr/local/bin may need sudo on fresh systems. Symlink into the bundle's
# MacOS/ so the binary always runs under the .app's Info.plist.
ln -sf /Applications/mlink.app/Contents/MacOS/mlink /usr/local/bin/mlink

echo "mlink installed to /Applications/mlink.app"
echo "CLI symlinked to /usr/local/bin/mlink"
echo "First run will request Bluetooth permission."

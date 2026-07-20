#!/bin/sh
# Assemble Alacritree.app around the release binary.  A bundle is required on
# macOS: UNUserNotificationCenter (desktop notifications + click-to-focus)
# refuses to run in a process without a bundle identifier.  The inner binary
# stays terminal-launchable via Alacritree.app/Contents/MacOS/alacritree.
#
# Mirrors the root Makefile's `app` target (upstream alacritty's bundling),
# minus man pages, completions, and terminfo.
set -e

root="$(cd "$(dirname "$0")/../../.." && pwd)"
template="$root/alacritree/extra/osx/Alacritree.app"
app_dir="$root/target/release/osx"
app="$app_dir/Alacritree.app"

cargo build --manifest-path "$root/Cargo.toml" -p alacritree --release

rm -rf "$app"
mkdir -p "$app_dir"
cp -R "$template" "$app_dir/"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$root/target/release/alacritree" "$app/Contents/MacOS/"
# No alacritree icon yet; reuse upstream's under the name Info.plist expects.
cp "$root/extra/osx/Alacritty.app/Contents/Resources/alacritty.icns" \
    "$app/Contents/Resources/alacritree.icns"
codesign --force --deep --sign - "$app"
echo "Created $app"

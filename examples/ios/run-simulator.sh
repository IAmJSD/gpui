#!/bin/sh
# Builds a gpui example for the iOS Simulator, wraps it in an .app bundle
# and launches it on a booted simulator (booting one if needed).
#
#   examples/ios/run-simulator.sh [example] [--device "iPhone 17 Pro"] [--release]
#
# Environment: SIMULATOR_UDID selects a simulator by id instead of name.
set -eu

cd "$(dirname "$0")/../.."

example="hello_world"
device="${SIMULATOR_DEVICE:-iPhone 17 Pro}"
profile="debug"
cargo_flags=""
prev=""
for arg in "$@"; do
    case "$arg" in
        --release) profile="release"; cargo_flags="--release" ;;
        --device=*) device="${arg#--device=}" ;;
        --device) ;; # value follows
        *) if [ "$prev" = "--device" ]; then device="$arg"; else example="$arg"; fi ;;
    esac
    prev="$arg"
done

target="aarch64-apple-ios-sim"
case "$(uname -m)" in
    x86_64) target="x86_64-apple-ios" ;;
esac

echo "Building $example for $target ($profile)..."
cargo build --example "$example" --target "$target" $cargo_flags

app_dir="target/$target/$profile/bundles/$example.app"
rm -rf "$app_dir"
mkdir -p "$app_dir"
cp "target/$target/$profile/examples/$example" "$app_dir/$example"
bundle_id="dev.gpui.examples.$(echo "$example" | tr '_' '-')"
sed -e "s/__EXECUTABLE__/$example/g" \
    -e "s/__BUNDLE_ID__/$bundle_id/g" \
    -e "s/__NAME__/$example/g" \
    examples/ios/Info.plist > "$app_dir/Info.plist"

udid="${SIMULATOR_UDID:-}"
if [ -z "$udid" ]; then
    udid=$(xcrun simctl list devices booted -j | python3 -c '
import json, sys
devices = json.load(sys.stdin)["devices"]
booted = [d for runtime in devices.values() for d in runtime if d["state"] == "Booted"]
print(booted[0]["udid"] if booted else "")')
fi
if [ -z "$udid" ]; then
    udid=$(xcrun simctl list devices available -j | python3 -c '
import json, sys
name = sys.argv[1]
devices = json.load(sys.stdin)["devices"]
matches = [d for runtime in devices.values() for d in runtime if d["name"] == name]
if not matches:
    sys.exit("no simulator named %r; see xcrun simctl list devices" % name)
print(matches[-1]["udid"])' "$device")
    echo "Booting $device ($udid)..."
    xcrun simctl boot "$udid"
    xcrun simctl bootstatus "$udid" -b >/dev/null
fi
open -a Simulator >/dev/null 2>&1 || true

echo "Installing $app_dir..."
xcrun simctl install "$udid" "$app_dir"
echo "Launching $bundle_id..."
xcrun simctl launch --console-pty "$udid" "$bundle_id"

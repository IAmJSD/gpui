#!/bin/sh
# Builds a gpui example for Android, packages it as an APK and runs it on
# a connected device or a running emulator (creating and booting one if
# there is none), then follows its log.
#
#   examples/android/run-emulator.sh [example] [--release] [--headless]
#
# Environment:
#   ANDROID_HOME      the SDK (platform-tools, build-tools, platforms, and
#                     emulator + a system image to boot one); found under
#                     the usual locations when unset
#   ANDROID_NDK_HOME  the NDK; the newest under $ANDROID_HOME/ndk when unset
#   AVD_NAME          the emulator to create or boot (default: gpui)
#   EMULATOR_FLAGS    extra flags for the emulator
set -eu

cd "$(dirname "$0")/../.."

example="hello_world"
profile="debug"
cargo_flags=""
headless=""
for arg in "$@"; do
    case "$arg" in
        --release) profile="release"; cargo_flags="--release" ;;
        --headless) headless="1" ;;
        *) example="$arg" ;;
    esac
done

if [ -z "${ANDROID_HOME:-}" ]; then
    ANDROID_HOME="${ANDROID_SDK_ROOT:-}"
fi
if [ -z "$ANDROID_HOME" ]; then
    for candidate in "$HOME/Library/Android/sdk" "$HOME/Android/Sdk" \
        /opt/homebrew/share/android-commandlinetools \
        /usr/local/share/android-commandlinetools; do
        if [ -d "$candidate" ]; then
            ANDROID_HOME="$candidate"
            break
        fi
    done
fi
if [ ! -d "$ANDROID_HOME" ]; then
    echo "Set ANDROID_HOME to the Android SDK" >&2
    exit 1
fi
export ANDROID_HOME
if [ -z "${ANDROID_NDK_HOME:-}" ]; then
    ANDROID_NDK_HOME=$(ls -d "$ANDROID_HOME"/ndk/* 2>/dev/null | sort -V | tail -1)
fi
if [ ! -d "${ANDROID_NDK_HOME:-}" ]; then
    echo "No NDK: set ANDROID_NDK_HOME or install one with sdkmanager 'ndk;<version>'" >&2
    exit 1
fi
export ANDROID_NDK_HOME
build_tools=$(ls -d "$ANDROID_HOME"/build-tools/* 2>/dev/null | sort -V | tail -1)
platform_jar=$(ls "$ANDROID_HOME"/platforms/android-*/android.jar 2>/dev/null | sort -V | tail -1)
if [ -z "$build_tools" ] || [ -z "$platform_jar" ]; then
    echo "Install build-tools and a platform: sdkmanager 'build-tools;35.0.0' 'platforms;android-35'" >&2
    exit 1
fi
PATH="$ANDROID_HOME/platform-tools:$ANDROID_HOME/emulator:$ANDROID_HOME/cmdline-tools/latest/bin:$build_tools:$PATH"
export PATH

# The Rust target and the NDK's clang for it. The API level is the
# manifest's minSdkVersion.
host=$(ls "$ANDROID_NDK_HOME/toolchains/llvm/prebuilt")
toolchain="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/$host/bin"
api=30
case "$(uname -m)" in
    arm64|aarch64) abi="arm64-v8a"; target="aarch64-linux-android"; clang="aarch64-linux-android$api-clang" ;;
    *) abi="x86_64"; target="x86_64-linux-android"; clang="x86_64-linux-android$api-clang" ;;
esac
target_upper=$(echo "$target" | tr 'a-z-' 'A-Z_')
target_lower=$(echo "$target" | tr '-' '_')
export "CARGO_TARGET_${target_upper}_LINKER=$toolchain/$clang"
export "CC_${target_lower}=$toolchain/$clang"
export "CXX_${target_lower}=$toolchain/$clang++"
export "AR_${target_lower}=$toolchain/llvm-ar"

# NativeActivity loads a shared library, so each example that runs on
# Android has a second, `cdylib` target with the same source: the
# `<example>_android` entries in Cargo.toml.
lib_name="${example}_android"
echo "Building $lib_name for $target ($profile)..."
cargo build --example "$lib_name" --target "$target" $cargo_flags

lib="target/$target/$profile/examples/lib$lib_name.so"
if [ ! -f "$lib" ]; then
    echo "No $lib: add an '[[example]] name = \"$lib_name\"' target with crate-type = [\"cdylib\"] to Cargo.toml" >&2
    exit 1
fi

package="dev.gpui.examples.$example"
app_dir="target/$target/$profile/apk/$example"
rm -rf "$app_dir"
mkdir -p "$app_dir/lib/$abi"
cp "$lib" "$app_dir/lib/$abi/"
sed -e "s/__PACKAGE__/$package/g" \
    -e "s/__NAME__/$example/g" \
    -e "s/__LIB__/$lib_name/g" \
    examples/android/AndroidManifest.xml > "$app_dir/AndroidManifest.xml"

echo "Packaging $app_dir/$example.apk..."
aapt2 link -o "$app_dir/unaligned.apk" --manifest "$app_dir/AndroidManifest.xml" -I "$platform_jar"
(cd "$app_dir" && zip -q -r unaligned.apk lib)
zipalign -f -p 4 "$app_dir/unaligned.apk" "$app_dir/aligned.apk"
keystore="$HOME/.android/debug.keystore"
if [ ! -f "$keystore" ]; then
    mkdir -p "$HOME/.android"
    keytool -genkeypair -keystore "$keystore" -alias androiddebugkey \
        -storepass android -keypass android -keyalg RSA -keysize 2048 \
        -validity 10000 -dname "CN=Android Debug,O=Android,C=US" >/dev/null 2>&1
fi
apksigner sign --ks "$keystore" --ks-pass pass:android --ks-key-alias androiddebugkey \
    --key-pass pass:android --out "$app_dir/$example.apk" "$app_dir/aligned.apk"

# A device or a running emulator; otherwise boot one.
if ! adb devices | grep -q "device$"; then
    avd="${AVD_NAME:-gpui}"
    if ! emulator -list-avds | grep -qx "$avd"; then
        image=$(ls -d "$ANDROID_HOME"/system-images/android-*/google_apis/"$abi" 2>/dev/null | sort -V | tail -1)
        if [ -z "$image" ]; then
            echo "No system image: sdkmanager 'system-images;android-35;google_apis;$abi'" >&2
            exit 1
        fi
        image_id=$(echo "$image" | sed -e "s|.*/system-images/|system-images;|" -e "s|/|;|g")
        echo "Creating the $avd emulator from $image_id..."
        echo no | avdmanager create avd -n "$avd" -k "$image_id" -d pixel_7 >/dev/null
    fi
    echo "Booting the $avd emulator..."
    flags="-no-boot-anim -no-snapshot -feature Vulkan -gpu swiftshader_indirect ${EMULATOR_FLAGS:-}"
    if [ -n "$headless" ]; then
        flags="$flags -no-window -no-audio"
    fi
    # shellcheck disable=SC2086
    nohup emulator -avd "$avd" $flags >"target/emulator-$avd.log" 2>&1 &
    adb wait-for-device
    until [ "$(adb shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')" = "1" ]; do
        sleep 2
    done
fi

echo "Installing $package..."
adb install -r "$app_dir/$example.apk" >/dev/null
adb logcat -c
echo "Launching $package..."
adb shell am start -W -n "$package/android.app.NativeActivity" >/dev/null
# The app's own output (stdout, stderr and panics go to logcat through
# gpui) and the activity's messages.
adb logcat -v time gpui-stdout:V gpui-stderr:V NativeActivity:V AndroidRuntime:E DEBUG:V '*:S'

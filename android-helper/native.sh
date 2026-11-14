#!/usr/bin/env bash
# Builds Test Bed's PC connection (crates/ac-remote-android) for the phone app:
# build-native/lib/<abi>/libtestbed_remote.so, which build.sh puts in the APK.
# ABIS="arm64-v8a armeabi-v7a x86_64" picks the ABIs (default: the two ARM ones phones use).
# Needs: rustup with the Android targets, and the Android NDK (sdkmanager "ndk;29.0.14206865").
set -euo pipefail
cd "$(dirname "$0")"

SDK="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Android/Sdk}}"
NDK="${ANDROID_NDK_HOME:-$(ls -d "$SDK"/ndk/* 2>/dev/null | sort -V | tail -1)}"
[[ -d $NDK ]] || { echo "no Android NDK: sdkmanager \"ndk;29.0.14206865\"" >&2; exit 1; }
BIN="$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin"
# getifaddrs, which finding PCs on the network needs, came in Android 7.
API=24
export PATH="$HOME/.cargo/bin:$PATH"

# Rust target, ABI, clang prefix
TARGETS=(
    "aarch64-linux-android arm64-v8a aarch64-linux-android"
    "armv7-linux-androideabi armeabi-v7a armv7a-linux-androideabi"
    "x86_64-linux-android x86_64 x86_64-linux-android"
)
ABIS="${ABIS:-arm64-v8a armeabi-v7a}"
rm -rf build-native
for t in "${TARGETS[@]}"; do
    read -r target abi clang <<<"$t"
    [[ " $ABIS " != *" $abi "* ]] && continue
    up=$(echo "$target" | tr a-z- A-Z_)
    lo=$(echo "$target" | tr - _)
    export "CARGO_TARGET_${up}_LINKER=$BIN/${clang}${API}-clang"
    export "CC_${lo}=$BIN/${clang}${API}-clang"
    export "CXX_${lo}=$BIN/${clang}${API}-clang++"
    export "AR_${lo}=$BIN/llvm-ar"
    cargo build --release --locked -p ac-remote-android --target "$target" --target-dir ../target/android
    mkdir -p "build-native/lib/$abi"
    "$BIN/llvm-strip" -o "build-native/lib/$abi/libtestbed_remote.so" "../target/android/$target/release/libtestbed_remote.so"
done
ls -l build-native/lib/*/

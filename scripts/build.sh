#!/usr/bin/env bash
set -euo pipefail

# ==============================================================================
# Bli-RS 构建脚本 —— chroot 内交叉编译到 Android (aarch64)
#
# 为什么这样搭（写给三个月后的自己）：
#
# 1. Google 官方 NDK 只提供 linux-x86_64 host 版本，没有 aarch64 host。
#    chroot 是 aarch64 glibc，装不了官方 NDK。
# 2. Termux 自带的 clang 二进制本身就是 `Target: aarch64-unknown-linux-android24`
#    —— 它天然面向 Android 的 bionic/libc，而不是 glibc。
#    而且它能在 chroot 内直接执行（Android 内核 + /system/bin/linker64 可用）。
#    所以直接拿它当 Rust 的 Android linker。
# 3. Rust 侧用 rustup 的 aarch64-linux-android std（chroot 已安装）。
# 4. Skia（Slint 的渲染后端）在 build.rs 里硬编码 host_tag = "linux-x86_64"，
#    因此需要一个 NDK 目录结构骗过它。这里只提供它真正读取的东西：
#      - source.properties（取 Pkg.Revision 主版本号）
#      - toolchains/llvm/prebuilt/linux-x86_64/sysroot（--sysroot 路径）
#    Skia 的 C++ 本体不需要重新编译 —— 预编译包缓存在
#    target/*/build/skia-bindings-*/out/skia/ 里，会被自动复用。
#
# 用法:
#   bash scripts/build.sh          # 编译 + 打包 APK
#   bash scripts/build.sh --lib    # 只编译动态库
# ==============================================================================

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$PROJECT_ROOT"

TERMUX_BIN=/data/data/com.termux/files/usr/bin
ANDROID_TARGET=aarch64-linux-android
NDK_SHIM=/tmp/ndk-shim-r29

echo "==> [1/4] 准备编译环境..."
# 注意：不要把 Termux 的 bin 加到 PATH 前面。
# Termux 的 `cc` 是 `--target=aarch64-unknown-linux-android24` 的 clang，
# 一旦排到前面，宿主的 build script / proc-macro 编译都会用它，
# 进而去链接 bionic 的 libc/libgcc_s，报 "unable to find library -lgcc_s"。
# Android 目标链接器通过 .cargo/config.toml 里的绝对路径指定，不需要 PATH。
export CARGO_HOME=/root/.cargo
export RUSTUP_HOME=/root/.rustup
export ANDROID_HOME=/data/data/com.termux/files/home/Android/Sdk
export ANDROID_JAR="$ANDROID_HOME/platforms/android-35/android.jar"
export RUSTFLAGS="-L ./fake_libs"

if [ ! -f "$ANDROID_JAR" ]; then
    echo "[-] 错误: 找不到 android.jar: $ANDROID_JAR"
    exit 1
fi

# --- Skia 需要的 NDK 目录桩 ------------------------------------------------
# Skia 只从里面读版本号与 sysroot 路径，不实际使用 NDK 的工具链
# （真正的编译/链接全部由 Termux clang 完成）。
if [ ! -f "$NDK_SHIM/source.properties" ]; then
    echo "    创建 NDK 目录桩: $NDK_SHIM"
    mkdir -p "$NDK_SHIM/toolchains/llvm/prebuilt/linux-x86_64/sysroot"
    mkdir -p "$NDK_SHIM/sources/android/cpufeatures"
    printf 'Pkg.Revision = 29.0.14206865\n' > "$NDK_SHIM/source.properties"
fi
export ANDROID_NDK="$NDK_SHIM"

echo "    target   : $ANDROID_TARGET"
echo "    linker   : $TERMUX_BIN/clang ($("$TERMUX_BIN/clang" --version | head -1))"
echo "    rustc    : $("$CARGO_HOME/bin/rustc" -V)"

echo "==> [2/4] 编译动态库..."
"$CARGO_HOME/bin/cargo" build --lib --target "$ANDROID_TARGET" --release

SO_PATH="$PROJECT_ROOT/target/$ANDROID_TARGET/release/libbli_rs.so"
if [ ! -f "$SO_PATH" ]; then
    echo "[-] 错误: 未生成 $SO_PATH"
    exit 1
fi

echo "==> [3/4] 校验产物..."
INFO=$(file -b "$SO_PATH")
echo "    $INFO"
case "$INFO" in
    *"ARM aarch64"*) ;;
    *) echo "[-] 错误: 产物架构不是 aarch64"; exit 1 ;;
esac
case "$INFO" in
    *"for Android"*) ;;
    *) echo "[-] 错误: 产物不是 Android ABI（可能误用了 glibc 工具链）"; exit 1 ;;
esac
if ! nm -D --defined-only "$SO_PATH" | grep -q " T android_main"; then
    echo "[-] 错误: 缺少 android_main 导出符号"
    exit 1
fi
echo "    android_main: OK"
echo "    体积: $(du -h "$SO_PATH" | cut -f1)"

if [ "${1:-}" = "--lib" ]; then
    echo ""
    echo "==> 仅编译模式，跳过打包。产物: ${SO_PATH#$PROJECT_ROOT/}"
    exit 0
fi

echo "==> [4/4] 打包 APK..."
bash "$PROJECT_ROOT/scripts/package_apk.sh"

echo ""
echo "=================================================================="
echo "构建完成"
echo "  动态库: ${SO_PATH#$PROJECT_ROOT/}"
echo "  APK   : dist/Bli-RS-arm64-v8a.apk"
echo "=================================================================="

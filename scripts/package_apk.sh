#!/usr/bin/env bash
set -e

# ==============================================================================
# Bli-RS 原生极速打包脚本 (纯血 NativeActivity 构建器)
# 无需 Gradle，无需 Android Studio，无需 DEX，0 行 Java，全链路纯 ARM 原生！
# ==============================================================================

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$PROJECT_ROOT"

echo "==> [1/5] 准备打包环境..."
export PATH=/data/data/com.termux/files/usr/bin:$PATH
AAPT2="/data/data/com.termux/files/usr/bin/aapt2"
ZIPALIGN="/data/data/com.termux/files/usr/bin/zipalign"
APKSIGNER="/data/data/com.termux/files/usr/bin/apksigner"
ANDROID_JAR="/data/data/com.termux/files/home/Android/Sdk/platforms/android-35/android.jar"
KEYSTORE="$HOME/.android/debug.keystore"

BUILD_DIR="$PROJECT_ROOT/android/build"
DIST_DIR="$PROJECT_ROOT/dist"
rm -rf "$BUILD_DIR"
mkdir -p "$BUILD_DIR/compiled" "$BUILD_DIR/apk_root/lib/arm64-v8a" "$DIST_DIR"

echo "==> [2/5] 编译 Android 资源与清单..."
$AAPT2 compile --dir "$PROJECT_ROOT/android/res" -o "$BUILD_DIR/compiled/resources.zip"
$AAPT2 link -I "$ANDROID_JAR" \
    --manifest "$PROJECT_ROOT/android/AndroidManifest.xml" \
    -o "$BUILD_DIR/base.apk" \
    "$BUILD_DIR/compiled/resources.zip"

echo "==> [3/5] 注入纯血 Rust 与 C++ 运行时 aarch64 动态库 (0 压缩直通)..."
SO_PATH="$PROJECT_ROOT/target/release/libbli_rs.so"
LIBCXX_SO="/data/data/com.termux/files/usr/lib/libc++_shared.so"
if [ ! -f "$SO_PATH" ]; then
    SO_PATH="$PROJECT_ROOT/target/debug/libbli_rs.so"
fi

if [ ! -f "$SO_PATH" ]; then
    echo "[-] 错误: 未找到编译产物 libbli_rs.so，请先运行 cargo build --lib"
    exit 1
fi

python3 -c "
import zipfile
apk_path = '$BUILD_DIR/base.apk'
so_path = '$SO_PATH'
libcxx_path = '$LIBCXX_SO'
with zipfile.ZipFile(apk_path, 'a', compression=zipfile.ZIP_STORED) as z:
    z.write(so_path, 'lib/arm64-v8a/libbli_rs.so')
    z.write(libcxx_path, 'lib/arm64-v8a/libc++_shared.so')
"

echo "==> [4/5] 执行 4 字节与 4KB 页面内存对齐 (zipalign)..."
$ZIPALIGN -p -f 4 "$BUILD_DIR/base.apk" "$BUILD_DIR/aligned.apk"

echo "==> [5/5] 执行 APK 签名 (apksigner V2+V3)..."
OUT_APK="$DIST_DIR/Bli-RS-arm64-v8a.apk"
$APKSIGNER sign \
    --ks "$KEYSTORE" \
    --ks-pass pass:android \
    --key-pass pass:android \
    --out "$OUT_APK" \
    "$BUILD_DIR/aligned.apk"

echo "==> [验证] 校验签名完整性..."
$APKSIGNER verify -v "$OUT_APK"

echo ""
echo "=================================================================="
echo "🎉 恭喜！纯血 Rust APK 已打包完成！"
echo "产物路径: $OUT_APK"
echo "安装命令: pm install -r $OUT_APK"
echo "=================================================================="

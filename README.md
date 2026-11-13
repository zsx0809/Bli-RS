# Bli-RS

> 纯血 Rust 实现的高性能、零发热、零 JVM 的 Bilibili Android 客户端。

---

## 当前进度

| 模块 | 状态 | 说明 |
|---|---|---|
| UI 骨架（Slint + Material 3 + Monet） | ✅ 完成 | 双列网格、胶囊 Tab、抽屉 |
| 手势系统（GestureArea） | ✅ 完成 | 自判位移，tap/pan 严格互斥 |
| 二级页（详情 + 播放） | ✅ 跑通 | 简介/评论 Tab、五联条、相关推荐 |
| 边缘返回 / 系统返回键 | ✅ 完成 | 左边缘归 App，返回键按栈序消费 |
| 推荐流真实数据 | ✅ 完成 | Wbi 签名 + 分页加载 + 封面懒加载 |
| 视频播放（画面） | ✅ 跑通 | 真实 B 站视频出画面，零拷贝硬解 |
| 视频播放（音频） | ⬜ 未做 | 需要 AAudio + 音频轨解码 |
| 播放器控制 | ⬜ 未做 | 暂停 / 进度 / seek / 倍速 / 横屏 |
| 搜索 | ⬜ 未做 | 顶栏搜索框目前是摆设 |
| 直播 / 关注 | ⬜ 未做 | 两个 Tab 是空态占位 |
| 登录 | ⬜ 未做 | 匿名访问，未登录只能拿 480P |

---

## 技术栈

- **UI**：Slint 1.18（实际渲染后端是 **Skia**，不是 femtovg）
- **网络**：reqwest 0.12 + rustls（纯 Rust TLS，交叉编译不需要 OpenSSL）
- **媒体**：Android NDK 原生 —— `AMediaExtractor` / `AMediaCodec` / `SurfaceView`
- **异步**：tokio（每个后台任务自建 current_thread runtime）
- **构建**：GitHub Actions（NDK r26d）+ `aapt2`/`zipalign`/`apksigner`，无 Gradle，无 DEX，0 行 Java

---

## 架构

```
                    ┌─────────────────────────────────┐
                    │  Slint UI（Skia 渲染，NativeActivity）│
                    └──────────────┬──────────────────┘
                                   │ 回调 / 属性
                    ┌──────────────▼──────────────────┐
                    │           src/feed.rs            │  推荐流桥接
                    │         src/cover.rs             │  封面下载解码
                    │         src/player.rs            │  播放编排
                    └──────────────┬──────────────────┘
                                   │
                    ┌──────────────▼──────────────────┐
                    │      crates/bli-protocol         │  协议层（无 UI 依赖）
                    │  Wbi 签名 / playurl / 推荐流      │
                    └──────────────┬──────────────────┘
                                   │
                    ┌──────────────▼──────────────────┐
                    │   NDK 原生（MediaExtractor/Codec） │
                    └─────────────────────────────────┘
```

**关键设计：协议层是独立 crate。**
它不依赖 Slint，可以在 chroot 里单独跑测试和 probe 验证接口，
不必为了测一个 HTTP 请求去链接 fontconfig。

---

## 播放链路（已跑通）

```
点击卡片
  → BiliClient::video_cid(bvid)      拿 cid
  → BiliClient::play_url(bvid, cid)  拿 DASH 轨道（6 视频 + 3 音频）
  → BiliClient::download_bytes(url)  下载 fMP4（77MB，约 70 秒）
  → AMediaExtractor_setDataSourceFd  解封装（fd 模式）
  → MediaCodec::configure(→Surface)  硬解
  → releaseOutputBuffer(render=true) 送显
  → SurfaceFlinger 合成上屏
```

全程 CPU 只搬运压缩码流，像素数据从解码器直接进 GPU 纹理，**零回读**。

---

## 踩过的坑（改代码前必读）

这些都是实测得出的结论，不是推测。改回去会立刻出问题。

### 1. `AMediaExtractor` 只能用 fd，不能用路径

```rust
// ❌ 永远返回 -10002（UNKNOWN_ERROR），与权限/格式无关
AMediaExtractor_setDataSource(extractor, "/path/to/file.m4s");

// ✅
AMediaExtractor_setDataSourceFd(extractor, fd, 0, len);
```

字符串路径版本内部依赖 JNI 做路径解析，纯 native 线程没有 `JNIEnv`。
实测连系统自带的普通 mp4 都打不开。

### 2. 视频 CDN 必须用 Range 请求

```
无 cookie    → 567
带 buvid3    → 567
带 Range     → 206  ✓
```

整文件请求被 CDN 当成爬虫拒绝。

### 3. 视频下载不能自动解压

CDN 在 Range 响应上带 `Content-Encoding` 头，但 body 是裸字节。
reqwest 自动解压会报 `error decoding response body`。

必须：
- 发送 `Accept-Encoding: identity`
- 不要调用 `.gzip()` / `.brotli()`

### 4. buvid 必须全程一致

`playurl` 返回的 URL 里嵌了当时的 buvid，CDN 会校验。
**整个播放流程必须复用同一个 `BiliClient` 实例**，
中途新建 client 会生成新 buvid，签名对不上直接 567。

### 5. SurfaceView 只有 `VISIBLE` 能拿到 Surface

```
GONE(8)       → 不参与布局，Surface 永不创建
INVISIBLE(4)  → Surface 对象存在，但 isValid() 永远 false
VISIBLE(0)    → 正常  ✓
```

但它一旦 `VISIBLE` 就会盖住 Slint UI（后加的 View 在上层），
所以**只能在播放时创建**，不能启动时创建。

### 6. View 操作必须在 Java 主线程

后台线程调 `addView` 会抛 `CalledFromWrongThreadException`
并**直接杀进程**（不可捕获）。

用 `AndroidApp::run_on_java_main_thread` 调度。

### 7. Slint 的类型大多不是 Send

```rust
Send      : Weak<AppWindow>, String, i64, Vec<纯数据>
NOT Send  : AppWindow, Image, ModelRc, VecModel, Rc<...>
```

最隐蔽的是 `Image`——它是 `VRc<OpaqueImageVTable>`。
所以含 `cover: image` 字段的 `VideoItemData` 整体也不能跨线程。

数据流被拆成两段：
```
后台线程 --(Vec<RawItem> 纯字符串)--> UI 线程构建 VideoItemData
后台线程 --(bvid, w, h, rgba 字节)--> UI 线程构建 Image 并回填
```

### 8. 封面回填必须持有同一个模型实例

对 `in-out property <[...]>` 这种值类型属性，
每次 `get_rows()` 可能拿到新的包装，对它 `set_row_data` 不反映到 UI。

必须把 `Rc<VecModel>` 存下来，属性指向的就是这个实例。

### 9. `slint::Timer` 必须在事件循环启动后创建

在 `main_window.run()` 之前建 Timer 不会触发。
现在的做法：在第一次 `set_rows` 的 `upgrade_in_event_loop` 闭包里启动。

### 10. `TouchArea.clicked` 不判位移（两个手势 bug 的总根因）

`input_items.rs` 的 `Released` 分支只检查「pressed && 抬起点在自己范围内」，
**中间滑了多远完全不管**。

后果：在列表上侧滑时，起点与终点通常在同一张卡片内，
于是一次侧滑被翻译成一次点击 —— 表现为「右滑直接切到下一个视频」。

另外，任何铺满页面的 `TouchArea` 都会吃掉内部元素的 `press`，
使卡片内的按钮（评论点赞、Tab）永久失效 —— 这是「评论区点不动」。

**现在的做法**：`ui/gesture.slint` 的 `GestureArea`。
继承 `TouchArea` 但不用它的 `clicked`，只接管 `pointer-event`：

- 位移 < `tap_slop`(8px) → `tap`
- 位移 ≥ `tap_slop` → 锁定主轴，只发 `pan_*`，**永不发 tap**
- 锁定在单次手势内不可逆

`pan` 沿组件树冒泡：纵向 → 自己滚，横向 → 冒泡到 `AppWindow` 切频道。
**没有任何覆盖层**，谁被按下谁处理。

### 11. 左边缘侧滑不能交还系统

进二级页时若调 `set_exclusion(32, false)`，左边缘就归系统了，
边缘侧滑 = 系统返回 = `finish()` 掉 `NativeActivity` = **App 直接退出**。

现在的做法：排除区全程生效；详情页的左边缘返回由自己的 `EdgeBack`
手势处理（从 `x=0` 起手，`vertical_enabled: false`，>32px 才返回）。

系统返回键则由 `AppWindow` 上的 `FocusScope` 接住，
按栈序消费（抽屉 → 详情页 → 交还系统）。

### 12. 滚动不用 Flickable

`Flickable` 的 press 是 `DelayForwarding(100ms)`：按下先扣住，
100ms 内没移动才交给子元素。内部按钮的按下反馈会**发粘**；
快速横滑时子元素**根本收不到 down**。

它也的确会 `content_y.remove_binding()`，外部别绑它的 `content-y`。

现在的做法：位移由 `scroll_y` 自管，拖动中 `duration: 0ms` 严格跟手，
抬手后惯性用 8ms Timer 推进（8ms < 90Hz 的 11.1ms，每帧都有新内容）。

### 13. 帧率必须按真实刷新率对齐

这台设备支持 60/90/120Hz，系统会动态切换。
手动逐帧累加惯性时，`Timer(16ms)` 在 90Hz 下会让 1/3 的帧没有新内容，
肉眼就是「一顿一顿」。

现在的做法：抬手后算出目标位置，交给 `animate y` 由渲染引擎插值，
刷新率是多少就滑多少次。

---

## 编译与打包

### GitHub Actions（现行方案）

本机是 Termux 原生环境，chroot 已卸载，`rustup` 无可用 toolchain，
Skia 也不适合在设备上现编。**构建全部交给 CI**：

```bash
git push origin main     # 自动触发
# 产物：Actions → 最新 run → Artifacts → Bli-RS-arm64-v8a
```

workflow 用官方 NDK r26d + `dtolnay/rust-toolchain`，
打包仍走 `scripts/package_apk.sh`（aapt2 + zipalign + apksigner，无 Gradle、无 DEX）。

三个已踩过的 CI 坑：

1. **`.cargo/config.toml` 不能写死 Termux 的 linker 路径**
   （会报 `failed to find tool "aarch64-linux-android-clang"`）。
   linker 走 `CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER` 环境变量。
2. **cc-rs 需要 `CC_` / `AR_`**
   （`ring` 之类 C 依赖自己不会去 NDK 里找 clang）：
   ```
   CC_aarch64_linux_android=<ndk>/bin/aarch64-linux-android26-clang
   AR_aarch64_linux_android=<ndk>/bin/llvm-ar
   ```
3. **`${{ env.ANDROID_HOME }}` 取不到 runner 的 SDK 路径**，
   要在 shell 里解析后写进 `$GITHUB_ENV`。

`scripts/package_apk.sh` 支持环境变量覆盖（`AAPT2` / `ZIPALIGN` / `APKSIGNER` /
`ANDROID_JAR` / `KEYSTORE` / `LIBCXX_SO` / `SKIP_PATCHELF`），
所以同一个脚本在 Termux 与 CI 上都能跑。

### Termux 本地（仅限已有工具链时）

```bash
export ANDROID_HOME=/data/data/com.termux/files/home/Android/Sdk
export ANDROID_JAR=$ANDROID_HOME/platforms/android-35/android.jar
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER=/data/data/com.termux/files/usr/bin/clang
export RUSTFLAGS="-L ./fake_libs"
cargo build --lib --release --target aarch64-linux-android
bash scripts/package_apk.sh
```

### 安装

```bash
su -c "pm uninstall com.bli.rs"   # 签名变化时需要先卸载
su -c "pm install -r dist/Bli-RS-arm64-v8a.apk"
su -c "am start -n com.bli.rs/android.app.NativeActivity"
```

### 协议层单独验证

协议层可以脱离 UI 单独跑：

```bash
cargo test -p bli-protocol                              # 14 个单元测试
cargo run -p bli-protocol --example probe               # 推荐流探针
cargo run -p bli-protocol --example playurl_probe       # 播放地址探针
```

---

## 目录结构

```text
BLI-RS/
├── crates/bli-protocol/       # 协议层（零 UI 依赖，可独立测试）
│   ├── src/api.rs             # 接口常量
│   ├── src/client.rs          # HTTP 客户端（含 download_bytes）
│   ├── src/wbi.rs             # Wbi 签名
│   ├── src/recommend.rs       # 推荐流 + av/bv 转换
│   ├── src/playurl.rs         # DASH 播放地址 + 轨道选择
│   └── examples/              # 可在 chroot 里直接跑的探针
├── src/
│   ├── lib.rs                 # android_main 入口，各回调接线
│   ├── feed.rs                # 推荐流桥接（线程模型最复杂的地方）
│   ├── cover.rs               # 封面下载 + LRU 缓存
│   ├── player.rs              # 播放编排（cid → playurl → 下载 → 解码）
│   ├── video_decoder.rs       # MediaExtractor + MediaCodec
│   ├── video_surface.rs       # JNI 创建 SurfaceView，拿 ANativeWindow
│   ├── video_layer.rs         # ASurfaceControl 封装（暂未使用，见下）
│   ├── android_window.rs      # JNI 上下文 + Monet 取色 + 手势排除区
│   └── mxprobe.rs             # 日志出口（__android_log_write）
├── ui/
│   ├── theme.slint            # Monet 调色板 + 布局常量
│   ├── appwindow.slint        # 主容器 + 统一手势层
│   ├── components/            # topbar / drawer / video_card / icons
│   └── views/                 # recommend / live / follow
├── scripts/
│   ├── build.sh               # 完整构建（推荐入口）
│   └── package_apk.sh         # aapt2 + zipalign + apksigner
├── android/                   # AndroidManifest + res（hasCode=false）
├── fake_libs/                 # 链接器静态桩（绕开 libc++ 静态库）
├── dist/                      # 输出 APK
└── docs/adr/                  # 架构决策记录
```

---

## 已知的未完成 / 待改进

**`src/video_layer.rs` 目前是死代码。**
它封装了 `ASurfaceControl`（可以在窗口下挂独立合成层，理论性能更好），
但 `AMediaCodec` 没有 `getOutputImage`，拿不到 `AHardwareBuffer`，
所以没法把解码结果喂给它。现在的方案是 `SurfaceView` + `setOutputSurface`，
一样是零拷贝，够用。保留这个文件是给以后留条路。

**点击命中测试是算出来的，不是 Slint 给的。**
`RecommendView::tap_at()` 用行列公式反推点中了哪张卡。
卡片布局改了（比如加入更多元素、不同行高）就要同步改这个函数。

**下载是整段拉，不是流式。**
77MB 一个视频要下 70 秒才能开始播。真正的做法是自己解析 `moof`
并按 Range 分段请求，边下边解。这是下一步该优化的点。

**音频还没接。**
现在只有画面没声音。

---

## 上游参考

- **本地源码**：`/data/data/com.termux/files/home/projects/PiliPlus`
- **GitHub**：[PiliPlus](https://github.com/bggRGjQaUbCoE/PiliPlus)（Flutter 实现）
- **协议参考**：[bilibili-API-collect](https://github.com/SocialSisterYi/bilibili-API-collect)

映射关系：

| 我们 | 上游 |
|---|---|
| `crates/bli-protocol/src/wbi.rs` | `lib/utils/wbi_sign.dart` |
| `crates/bli-protocol/src/recommend.rs` | `lib/http/video.dart` + `lib/models/home/rcmd/result.dart` |
| `crates/bli-protocol/src/playurl.rs` | `lib/http/video.dart` + `lib/models/video/play/url.dart` |
| `src/player.rs` 播放编排 | `lib/pages/video/controller.dart` |

---

## 架构决策记录

👉 [ADR-0001: 架构设计决策记录](docs/adr/ADR-0001-architecture-design.md)

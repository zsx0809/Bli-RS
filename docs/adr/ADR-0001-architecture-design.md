# ADR-0001: Bli-RS 纯血架构设计与技术决策记录

> **上游参考源码路径 (Reference Source Code)**:  
> `/data/data/com.termux/files/home/projects/PiliPlus`
>
> **GitHub 远程仓库**:  
> `https://github.com/zsx0809/Bli-RS`

---

## 1. 架构目标与设计原则 (Context & Goals)

官方客户端与传统混合开发端存在严重的**后台滥用、遥测泄漏、内存膨胀与音视频软解死循环**，实测导致外壳温度突破 **42.0°C**，电池温度突破 **41.1°C**。

**Bli-RS 核心原则：**
1. **纯血零虚拟机 (Zero JVM/ART)**：全栈纯 Rust 实现，通过 Android 原生 `NativeActivity`（`android:hasCode="false"`）由操作系统内核直接加载，0 行 Java，0 个 DEX。
2. **零拷贝硬件直解**：视频流通过 NDK 纯 C 接口 `AMediaCodec` + `ANativeWindow` 直通 GPU Overlay；音频通过 `AAudio` 直通系统混音器。
3. **极简 Material 3 原生美学**：融合 Monet 动态壁纸取色引擎与 AOSP 物理动力学，彻底剔除一切带货广告、直播悬浮与推荐流污染。

---

## 2. 核心技术决策 (Architectural Decisions)

### 2.1 表现层 UI 选型：Slint UI 框架
- **保留模式 (Retained Mode)**：Slint 编译期将 DSL 转译为 Rust 状态机。屏幕静止时渲染线程完全休眠，CPU 占用为 0%，根除发热。
- **布局规范**：
  - 首页：顶部三段式胶囊 Tab 切换器（`[直播]  [推荐]  [关注]`），去除底部导航栏以最大化垂直显示面积。
  - 推荐流：采用经典 **双列瀑布流网格（2 Columns Grid）**，上下结构（封面+数据徽章+标题+UP主）。
  - 侧边抽屉面板（Navigation Drawer）：承载个人中心、UID、硬币状态、离线缓存及“NDK 硬件解码设置”，支持左边缘手势右拉呼出。
- **手势动力学**：
  - 顶部导航区域支持横向轻扫手势切换 Tab。
  - 屏幕左边缘（0~28px）支持向右拉动直接唤出抽屉。

### 2.2 协议层与反爬策略 (`bli-proto`)
参考 PiliPlus 源码（`/data/data/com.termux/files/home/projects/PiliPlus`）移植：
- **Wbi 密钥混淆签名**：逆向算法移植至纯 Rust（`tokio` + `reqwest` + `md-5`），完全替代官方繁杂的 Java Native 校验。
- **Dash 音视频分流重组**：分别提取 1080P/4K 视频轨道与高码率音频轨道，送入底层音视频同步时钟。
- **gRPC Protobuf 编解码**：使用 `prost` 编译 B 站官方 Proto 定义，纯内存零拷贝反序列化推荐流与动态。

### 2.3 编译与打包流水线 (Build & Packaging Pipeline)
已在本地 Termux（aarch64 原生环境）验证并固化极速构建链：
- **Rust 编译**：
  ```bash
  export PATH=/data/data/com.termux/files/usr/bin:$PATH
  export RUSTFLAGS="-L ./fake_libs"
  cargo build --lib --release
  ```
- **ELF 动态链接节修补**：
  使用 `patchelf --add-needed libc++_shared.so target/release/libbli_rs.so` 确保 Android 动态链接器正确加载 C++ 运行时。
- **纯血零 DEX 原生打包 (`scripts/package_apk.sh`)**：
  1. 原生 `aapt2` 编译 Android 清单与基础资源；
  2. 采用 Python 0 压缩（`ZIP_STORED`）注入 `libbli_rs.so` 与 `libc++_shared.so`；
  3. 原生 ARM 版 `zipalign -p -f 4` 实施 4KB 页面内存对齐；
  4. 原生 `apksigner` 通过 `debug.keystore` 签名（V2+V3）。

---

## 3. 项目演进路线图 (Milestones)

- [x] **Milestone 1**: 确定纯血架构，完成 Slint UI 双列网格与手势抽屉，跑通本地构建与真机安装启动（133ms 瞬开）。
- [x] **Milestone 2**: 构建 `crates/bli-protocol` crate，移植 Wbi 签名、首页推荐流、DASH 播放地址接口。推荐流已接入真实数据并支持分页。
- [x] **Milestone 3（部分）**: 接入 `AMediaExtractor` + `AMediaCodec` + `SurfaceView` 硬解，**视频画面已跑通**。音频、播放控制、弹幕尚未做。
- [ ] **Milestone 4**: 接入登录认证（QR Code / Cookie 管理）、个人动态与关注列表。
- [ ] **Milestone 5**: 流式播放（解析 `moof` 分段按需 Range 请求，替代整段下载）。

---

## 4. 实装修正记录 (Implementation Corrections)

以下条目均在真机（1240×2772 @408dpi，Android 14）上验证，修正了 ADR 初稿中的错误判断。

### 4.1 双列卡片宽度：必须由父级显式传入

**问题**：两列卡片宽度不一致，实测同一屏内为 `272dp / 230dp`，且每行比例都不同。

**根因**：`VideoCard` 放在 `HorizontalLayout` 里时没有宽度约束，Slint 按**内容首选宽**分配空间，标题长的卡片自然更宽。

**修正**：列宽由视口宽度显式计算后传入：

```slint
property <length> col_w:
    Math.floor(((width - page_margin*2 - grid_gap) / 2) / 1px) * 1px;
```

`page_margin == grid_gap == 12px`，保证页边距与列间距同值、左右对称。

### 4.2 标题两行截断：`vertical-alignment: top` 在 Skia 后端会偏移一行

**问题**：`wrap: word-wrap` + `max-lines: 2` 的标题只显示第二行，第一行跑到元素框外。

**验证过程**：同一位置并排三个变体（center / top+44px / 不换行）对照，结论明确——
内容为 2 行时，Skia 后端在 `vertical-alignment: top` 下会额外向上偏移一个行高。

**修正**：改用 `vertical-alignment: center`，元素高度设为两行行高 + 余量（46px）。
已验证两行完整显示。

> 注：这不是断行问题。构建中 `unicode-linebreak` 特性已启用，中文断行正常。

### 4.3 手势仲裁：`SwipeGestureHandler` 会阻断内部滚动

**问题**：用 `SwipeGestureHandler` 包住页面容器做全屏左右滑切 Tab，导致列表完全无法纵向滚动。

**根因**（读 `i-slint-core/items/input_items.rs`）：
`SwipeGestureHandler` 在"按住但未超过阈值"阶段返回 `InputEventFilterResult::ForwardAndInterceptGrab`，
**抢走内部 `Flickable` 的 grab**，滚动因此永远起不来。

`TouchArea` 的问题更严重——直接吞掉全部 press。

**修正**：切页手势改由左右两侧各 64dp 的手势条（`TouchArea`）承担，
页面容器交给 `Flickable` 自身负责滚动。两者区域不重叠，纵向滚动恢复正常。

### 4.4 系统返回手势与左边缘抽屉手势冲突

**问题**：从 `x=6px` 起手右滑，App 被系统切到后台。

**方案**：通过 JNI 调用 `View.setSystemGestureExclusionRects()` 把左边缘 32dp 排除出系统手势区。
本项目 `hasCode=false`，故走 JNI 反射，不引入 Java 类。

**实装状态**：JNI 通道已打通（`src/android_window.rs`），调用成功无异常，
但 `NativeActivity` 的窗口装饰视图层级导致排除区未被系统采信，
`dumpsys` 中 `EdgeBackGestureHandler` 仍接管边缘区域。

**当前策略**：抽屉主入口为**点击左上角头像**（已验证可用），
左边缘手势条保留作为辅助，在未启用系统手势导航的设备上可用。

### 4.5 构建环境：文档中的两条命令不完整

README 里写的 `export RUSTFLAGS="-L ./fake_libs"` + `cargo build --lib --release` 不足以构建成功。
实际需要额外提供 Android SDK 路径，否则 `i-slint-backend-android-activity` 的 build script
会在 `android_build::android_jar(None)` 处 panic（`No Android platforms found`）：

```bash
export ANDROID_HOME=/data/data/com.termux/files/home/Android/Sdk
export ANDROID_JAR=$ANDROID_HOME/platforms/android-35/android.jar
export RUSTFLAGS="-L ./fake_libs"
cargo build --lib --release
```

另注：`skia-bindings` 的预编译二进制从 GitHub 下载，
如果网络不通会回退到全量源码编译并因缺少 `ANDROID_NDK` 失败。
`target/release/build/skia-bindings-*/out/skia` 中已有缓存时可正常复用。

### 4.6 封面 CDN 无防盗链

初版判断"封面空白是因为 B 站图床校验 Referer"是**错的**。实测：

```
无 Referer  → 200, 575361 bytes
有 Referer  → 200, 575361 bytes
```

封面为空只是因为 `cover_url` 字段本身是空字符串（骨架阶段占位数据）。

---

## 5. 播放器实装修正记录

这一节记录打通「点击卡片 → 画面出现」过程中，靠实测而非推测得出的结论。
每一条都是踩过的坑，改代码前必读。

### 5.1 `AMediaExtractor` 只能用 fd，不能用路径

```rust
// ❌ 一律返回 -10002 (UNKNOWN_ERROR)
AMediaExtractor_setDataSource(e, "/path/to.m4s");

// ✅
AMediaExtractor_setDataSourceFd(e, fd, 0, len);
```

字符串路径版本内部依赖 JNI 做路径解析，纯 native 线程没有 `JNIEnv`。
实测连系统自带的普通 mp4、以及放在 `/sdcard` 且有读权限的文件都打不开，
**与权限、容器格式无关，纯粹是调用路径不对**。

### 5.2 视频 CDN 必须用 Range 请求

同一 URL 的四种请求方式实测：

```
无 cookie          → 567
带 buvid3 cookie   → 567
带完整 cookie      → 567
带 Range: bytes=0- → 206  Partial Content  ✓
```

整文件请求被 CDN 当成爬虫。必须发 Range。

### 5.3 视频下载不能启用自动解压

CDN 在 Range 响应上带了 `Content-Encoding` 头，但 body 是未压缩的原始字节。
reqwest 的自动解压会尝试按 gzip/br 解码，报 `error decoding response body`。

正确做法：另建一个**不调用** `.gzip()` / `.brotli()` 的 client，
并显式发送 `Accept-Encoding: identity`。

注意这与 JSON 接口相反 —— 那些接口需要开启压缩。

### 5.4 buvid 必须全程一致

`playurl` 返回的 CDN URL 里嵌了请求时的 buvid（形如
`...&buvid=95A6E0FA-...29644infoc&...`），CDN 会校验它。

**整个播放流程必须复用同一个 `BiliClient` 实例**。中途新建 client
会生成新 buvid，签名对不上直接 567。

### 5.5 SurfaceView 只有 `VISIBLE` 能拿到有效 Surface

```
GONE(8)      → View 不参与布局 → Surface 永不创建，getSurface() 返回 null
INVISIBLE(4) → 参与布局但不绘制 → Surface 对象存在，isValid() 永远 false
VISIBLE(0)   → 正常  ✓
```

但 `VISIBLE` 的 SurfaceView 会盖住 Slint 的 UI（后加的 View 在上层），
所以**只能在播放时创建，不能在启动时创建**。

### 5.6 View 操作必须在 Java 主线程

后台线程调 `ViewGroup.addView` 会抛 `CalledFromWrongThreadException`，
且**直接杀进程**（这是 ViewRootImpl 的行为，不是可捕获的业务异常）。

调度方式：`AndroidApp::run_on_java_main_thread()`。
需要在 `android_main` 早期把 `AndroidApp` 存起来（内部是 `Arc<RwLock>`，clone 便宜）。

### 5.7 JNI 调接口方法会失败

`SurfaceView.getHolder()` 返回的是接口 `SurfaceHolder`，
对它调 `getSurface()` 用 jni crate 的 `call_method` 会 `NoSuchMethodError`。

原因：`GetMethodID` 对实现类查找时不遍历接口方法表。

解法（当前采用）：走反射
`SurfaceHolder.class.getMethod("getSurface").invoke(holder)`。

### 5.8 Slint 的 `Image` 不是 Send

`Image` 内部是 `VRc<OpaqueImageVTable>`，不实现 `Send`。
所以含 `cover: image` 字段的 `VideoItemData` 整体也不能跨线程。

数据流因此拆成两段纯数据传递：

```
后台线程 --(Vec<RawItem> 纯字符串)-----> UI 线程构建 VideoItemData
后台线程 --(bvid, w, h, rgba 字节)-----> UI 线程构建 Image 并回填
```

### 5.9 封面回填必须持有模型实例本身

对 `in-out property <[...]>` 这类值类型属性，每次 `get_rows()`
可能返回新的 `ModelRc` 包装，对副本 `set_row_data` 不反映到 UI
（表现为只有前一两张封面能显示）。

必须把 `Rc<VecModel>` 存进 `thread_local`，让窗口属性指向同一个实例。

另：封面队列必须是全局 `static` 而非 `thread_local` ——
写入在下载线程、读取在 UI 线程，`thread_local` 会让两边看不到对方的数据。

### 5.10 `slint::Timer` 必须在事件循环启动后创建

在 `main_window.run()` 之前建 Timer 不会触发。
当前做法：在第一次 `set_rows` 的 `upgrade_in_event_loop` 闭包里启动（幂等）。

### 5.11 纵向滚动：自管 `scroll_y`，不用 `Flickable`（已重写）

> **2026-11 重写**：本节旧结论「Flickable 会 remove_binding 所以不能用」
> 只对了一半。真正的约束来自**与之配套的手势层**，见下面 5.13 的重写说明。

`Flickable` 确实会在触摸路径里调 `content_y.remove_binding()`
（`i-slint-core/items/flickable.rs`，press 与 wheel 两个分支都有），
把外部双向绑定拆掉。所以**外部不要再绑定它的 content-y**。

但更关键的是它的 press 转发策略：
`input_event_filter_before_children` 对 `MouseEvent::Pressed` 返回
`DelayForwarding(100ms)` —— 按下后先扣住，100ms 内没有移动才把 press
交给子元素；一旦移动超过 8px 且该轴可滚，就自己接管并给子元素补一个 Exit。

即：内部的 `TouchArea.clicked` 会有 **100ms 的按下延迟**，
快速横滑时子元素**根本收不到 down**。

当前方案（推荐流与详情页统一）：
- 不用 `Flickable`，纵向位移完全由 `scroll_y` 自管
- 拖动中 `duration: 0ms` 严格跟手；抬手后惯性用 8ms Timer 逐帧推进
  （8ms < 90Hz 的 11.1ms，每帧都有新内容）
- 点击与拖动由 `GestureArea` 区分（见 5.13）

### 5.12 帧率必须按真实刷新率对齐

设备支持 60/90/120Hz 动态切换（实测一度跑在 90Hz）。
手动逐帧累加惯性时，固定 `Timer(16ms)` 在 90Hz 下会有 1/3 的 vsync
拿不到新内容，肉眼就是「一顿一顿」。

当前方案：
- 拖动中 `scroll_animating = false`（duration 0ms，严格跟手）
- 抬手后算出目标位置，置 `true` 走 260ms `ease-out`
- 由渲染引擎按真实 vsync 插值

### 5.13 手势：`TouchArea.clicked` 不判位移 —— 这是两个 bug 的总根因

> **2026-11 全量重写**。旧的「全屏手势层 + `tap_at` 坐标反推」方案已彻底删除。

**根因一：`clicked` 只看起终点，不看路径。**

`i-slint-core/items/input_items.rs` 的 `MouseEvent::Released` 分支只做两件事：

```rust
if *button == Left
    && Rect::new(Point::default(), geometry.size).contains(*position)
    && self.pressed()
{
    self.clicked().call(&());
}
```

没有位移判断。在列表上侧滑时，手指起点与终点通常落在**同一张卡片**内，
于是一次侧滑被翻译成一次点击 —— 表现为「右滑直接切到下一个视频」。

**根因二：覆盖层会吃掉内部的 down。**

旧的统一手势层是个铺满页面的 `TouchArea`，`press` 先到它手里，
卡片自己的 `TouchArea` 完全收不到 `clicked`。
为了绕开，旧代码让手势层判定「无位移 = 点击」并把坐标转发给
`RecommendView::tap_at()` 按行列公式反推命中了哪张卡。这套做法有三处硬伤：

1. **卡片内任何独立控件都不可能工作** —— 评论区的点赞、更多菜单、
   Tab 按钮全被这一层挡住，这是「评论区点不动」的直接原因。
2. 反推公式假设「窗口坐标 == 内容坐标」，列表滚动过 / 页面横移过之后
   两个坐标系不重合，命中必然错位。
3. 按压反馈、滑动取消、抬手命中是三套手工同步的状态，必然错位。

**当前方案：`ui/gesture.slint` 的 `GestureArea`，没有任何覆盖层。**

```slint
export component GestureArea inherits TouchArea { ... }
```

继承 `TouchArea` 但**不使用它的 `clicked`**，只接管 `pointer-event` 原始事件：

| 条件 | 结果 |
|---|---|
| 位移全程 < `tap_slop`（8px） | 发 `tap(x, y)` |
| 位移 ≥ `tap_slop` | 锁定主轴，此后只发 `pan_*`，**永不发 `tap`** |
| 长按计时到点且未锁定 | 发 `long_press` |

锁定在单次手势内**不可逆**：判定成拖动就再也不会变成点击，
判定成纵向就不会翻成横向。

**事件路由：谁被按下谁处理，`pan` 沿组件树往上冒泡。**

```
VideoCard.GestureArea ──tap──→ 打开视频
                      ──pan──→ RecommendView
                                  ├─ Vertical   → 自己滚动 + 惯性
                                  └─ Horizontal → 冒泡到 AppWindow 切频道
```

事件在哪一层开始就在那一层结束，没有跨层抢 grab，没有 100ms 延迟。

### 5.13b 左边缘侧滑不能交还系统

进入二级页时曾调 `set_exclusion(32, false)`，把左边缘 32dp 还给了系统手势区。
结果：边缘侧滑 = 系统返回 = `finish()` 掉 `NativeActivity` = **App 直接退出**。

当前方案：
- 排除区**全程生效**，不随页面切换解除
- 详情页的左边缘返回由 `EdgeBack`（`GestureArea`）自己处理，
  从真正的 `x=0` 起手，`vertical_enabled: false`（竖向起手放弃，不抢滚动），
  位移 > 32px 才返回

### 5.13c 系统返回键必须有 `FocusScope` 接住

`android-activity` 把 `Keycode.Back` 映射成 `Key.Back` 交给 Slint
（`androidwindowadapter.rs` 的 `map_key_code`）。若无人 `accept`，
它返回 `InputStatus::Unhandled` → 走 fallback → `finish` 掉 Activity。

所以 `AppWindow` 上挂了 `forward-focus` 到一个 `FocusScope`，
按栈序消费：抽屉打开 → 关抽屉；详情页 → 回首页；否则 `reject` 交还系统。

注意 Slint 的 callback 返回值是**最后一个表达式**，不能写 `return accept;`。

### 5.14 构建：chroot 交叉编译（已废弃，改用 GitHub Actions）

> **2026-11 更新**：本机 chroot 已卸载，以下方案**不再可用**，仅作历史记录。
> 现行方案见 5.15。

Google 官方 NDK 只有 `linux-x86_64` host，没有 aarch64 版本。
但 Termux 自带的 clang 二进制本身就是
`Target: aarch64-unknown-linux-android24`，且能在 chroot 内直接执行。

因此：
1. `.cargo/config.toml` 指定 `linker = "/data/data/com.termux/files/usr/bin/clang"`
2. `rustup target add aarch64-linux-android`
3. Skia 的 `build.rs` 硬编码 `host_tag = "linux-x86_64"`，
   需要一个 NDK 目录桩（只提供 `source.properties` 和空的 sysroot 路径）

注意：**不要把 Termux 的 bin 加到 PATH 前面**。
否则宿主的 build script / proc-macro 编译也会用那个 clang，
去链接 bionic 的 libc，报 `unable to find library -lgcc_s`。

### 5.15 构建：GitHub Actions（现行方案）

本机是 Termux 原生环境，chroot 已卸载，`rustup` 没有可用 toolchain，
Skia 也无法在设备上现编。因此构建全部交给 CI：

- workflow: `.github/workflows/android.yml`
- 用官方 NDK r26d + `dtolnay/rust-toolchain`
- 打包仍走 `scripts/package_apk.sh`（aapt2 + zipalign + apksigner，无 Gradle）

踩过的三个 CI 坑：

1. **`.cargo/config.toml` 不能写死 Termux 的 linker 绝对路径**。
   那会让 CI 报 `failed to find tool "aarch64-linux-android-clang"`。
   现在该文件只留 net/http 配置，linker 通过
   `CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER` 环境变量给。

2. **cc-rs 需要 `CC_` / `AR_` 环境变量**。
   rustc 只知道 linker；`ring` 这类 C 依赖由 cc-rs 驱动，
   它自己不会去 NDK 目录里找 clang。必须显式给：

   ```
   CC_aarch64_linux_android=<ndk>/bin/aarch64-linux-android26-clang
   AR_aarch64_linux_android=<ndk>/bin/llvm-ar
   ```

3. **`${{ env.ANDROID_HOME }}` 取不到 runner 的 SDK 路径**。
   `env` 上下文只含 workflow/job/step 里定义的值，不含 runner 环境变量。
   必须在 shell 里解析后写进 `$GITHUB_ENV`。

`scripts/package_apk.sh` 也因此改成可被环境变量覆盖：

| 变量 | 用途 |
|---|---|
| `AAPT2` / `ZIPALIGN` / `APKSIGNER` | 工具路径，默认走 PATH |
| `ANDROID_JAR` | 平台 jar |
| `KEYSTORE` | 签名用 keystore |
| `LIBCXX_SO` | libc++_shared.so 来源 |
| `SKIP_PATCHELF=1` | CI 上跳过 patchelf（NDK 已正确链接 DT_NEEDED） |

### 5.16 Slint 语法坑（编译器实测报错，不是猜的）

| 写法 | 报错 | 正确写法 |
|---|---|---|
| `property <real>` | `Unknown type 'real'` | `property <float>` |
| `dx + vx * 80px` | `Cannot convert (px²) to length` | 先 `/ 1px` 降到 float 域再乘 |
| `(scroll_y / 1px) - Spacing.page_margin` | `Cannot convert float to length` | `- (Spacing.page_margin / 1px)` |
| `return accept;` | — | callback 用最后一个表达式作返回值 |

### 5.17 真机验证记录（2026-11-13，1240x2772 @408dpi，Android 14）

修完后逐条实测：

| 操作 | 结果 |
|---|---|
| 首页正常渲染、封面加载 | ✅ |
| 点击卡片进详情页 | ✅ |
| 详情页纵向滚动（在相关卡片上拖也一样） | ✅ |
| 详情页点「评论」Tab 切换 | ✅ |
| 详情页左上返回按钮 | ✅ |
| **左边缘右滑返回首页，App 不退出** | ✅ |
| **在相关视频列表上右滑，不误切视频** | ✅ |

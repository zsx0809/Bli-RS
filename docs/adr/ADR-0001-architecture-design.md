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
- [ ] **Milestone 2**: 构建 `bli-proto` crate，从 PiliPlus 移植 Wbi 签名与首页推荐视频真实接口。
- [ ] **Milestone 3**: 接入 NDK `AMediaCodec` 与 `ANativeWindow` 硬解播放器，跑通 Dash 视频直链播放与弹幕渲染。
- [ ] **Milestone 4**: 接入登录认证（QR Code / Cookie 管理）、个人动态与关注列表。

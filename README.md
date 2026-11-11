# Bli-RS

> 纯血 Rust 实现的高性能、零发热、极简无臃肿 Bilibili Android 客户端。

---

## 🌟 核心架构亮点

1. **零 JVM / ART 依赖（纯血 Native）**：
   - 全栈由 Rust 驱动，基于 Android 原生 `NativeActivity`（`android:hasCode="false"`）。
   - **0 行 Java 代码，0 个 DEX 文件**，由 Linux 内核在启动时直接 `dlopen` 加载 ARM64 动态库，冷启动 Displayed 时间仅 **133 毫秒**。
2. **根除发热与后台常驻**：
   - 彻底告别官方客户端吃满大核的 `ijkservice` 后台软解死循环与臃肿遥测进程（实测手机温度从 42°C 降至室温）。
   - 采用保留模式（Retained Mode）UI 渲染，屏幕静止时 GPU 与 CPU 完全静默休眠。
3. **Material 3 原生设计语言**：
   - 首页精炼为 **`[ 直播 ]  [ 推荐 ]  [ 关注 ]`** 三段式 Tab，砍掉底部导航栏以最大化可视空间。
   - 经典 **双列瀑布流视频网格**（16:10 沉浸封面 + 播放量/弹幕徽章 + 标题与 UP 主）。
   - 完整支持 **左右轻扫切 Tab** 与 **屏幕左边缘右滑呼出抽屉面板**。

---

## 📂 源码与目录结构

```text
BLI-RS/
├── android/                   # Android 原生清单与资源
│   ├── AndroidManifest.xml   # 声明 NativeActivity (hasCode=false)
│   └── res/values/           # 基础资源与标题配置
├── dist/                      # 最终打包产物目录 (存放可安装 APK)
├── docs/                      # 架构设计与技术文档
│   └── adr/
│       └── ADR-0001-architecture-design.md # 核心架构决策记录
├── fake_libs/                 # 本地链接器静态桩目录
├── scripts/                   # 原生编译与打包脚本
│   └── package_apk.sh         # 极速打包签名器 (aapt2 + zipalign + apksigner)
├── src/                       # Rust 核心源代码
│   ├── lib.rs                 # 导出库与 Android android_main 入口
│   └── main.rs                # 桌面开发与测试入口
└── ui/                        # Slint 声明式 UI 表现层 (带保姆级 M3 注释)
    ├── components/
    │   ├── drawer.slint       # 侧滑抽屉 (个人中心、UID、硬件解码设置)
    │   ├── topbar.slint       # 顶栏三 Tab 切换器
    │   └── video_card.slint   # 双列竖版视频卡片
    ├── views/
    │   ├── follow_view.slint  # 关注动态视图
    │   ├── live_view.slint    # 直播专区视图
    │   └── recommend_view.slint # 经典双列瀑布流推荐页
    ├── appwindow.slint        # 主视窗容器与手势状态机
    └── theme.slint            # Monet 动态调色板与动力学设计变量
```

---

## 🔍 上游协议参考

- **本地参考源码路径**：`/data/data/com.termux/files/home/projects/PiliPlus`
- **GitHub 上游项目**：[PiliPlus (Flutter 开源客户端)](https://github.com/bggRGjQaUbCoE/PiliPlus)
- **协议移植映射表**：
  - Wbi 签名逆向逻辑：参考 `PiliPlus/lib/utils/wbi_sign.dart`
  - 推荐流与视频详情接口：参考 `PiliPlus/lib/http/`
  - 动态与 gRPC 编解码：参考 `PiliPlus/lib/grpc/`

---

## 🛠 本地编译与打包指南 (Termux 原生环境)

项目已配置闭环的原生编译链，无需 Android Studio，无需 Gradle，直接在手机本地 10 秒打出 APK：

### 1. 编译纯血动态库
```bash
export PATH=/data/data/com.termux/files/usr/bin:$PATH
export RUSTFLAGS="-L ./fake_libs"
cargo build --lib --release
```

### 2. 打包与自动签名
```bash
patchelf --add-needed libc++_shared.so target/release/libbli_rs.so
bash scripts/package_apk.sh
```

### 3. 一键安装并运行
```bash
su -c "pm install -r dist/Bli-RS-arm64-v8a.apk && am start -n com.bli.rs/android.app.NativeActivity"
```

---

## 📑 架构决策记录 (ADR)
详细技术选型、手势与渲染机制详见：  
👉 [ADR-0001: 架构设计决策记录](docs/adr/ADR-0001-architecture-design.md)

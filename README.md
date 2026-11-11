# Bli-RS

> 纯血 Rust 实现的高性能、零发热、极简无臃肿 Bilibili 客户端。

---

## 架构原则

1. **零 JVM / ART 依赖**：全栈由 Rust 驱动，通过 NativeActivity 与 Android NDK 纯 C API 直接与操作系统底层交互。
2. **零拷贝硬件直解**：音视频播放直连 `AMediaCodec` + `ANativeWindow` + `AAudio`，杜绝软解死循环与高热量。
3. **极简轻量 UI**：基于现代声明式 Rust UI 引擎（Slint）实现自适应布局与平滑触控惯性滚动。

---

## 协议与功能参考

- **上游参考源码**：[PiliPlus](https://github.com/bggRGjQaUbCoE/PiliPlus)（基于 Flutter 的优秀开源实现，用于提取 B 站反爬 Wbi 签名、gRPC 协议及 Dash 流媒体结构）。
- **本地参考路径**：`/data/data/com.termux/files/home/projects/PiliPlus`

---

## 编译与构建

本项目已配置全自动 GitHub Actions CI。代码推送到 `main` 分支后，云端将自动拉取 NDK 环境并调用 `cargo-apk` 编译生成支持 `aarch64-linux-android` 架构的原生 APK。

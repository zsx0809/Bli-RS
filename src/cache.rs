// ==============================================================================
// 缓存策略
//
// 设计目标（用户明确提出的那条）：
//   「单个视频循环播放 → 缓存保留，省流量；自动连播切下一个 → 立刻删上一个」
//
// 所以这里只有**一条**规则：任何时候磁盘上最多只有**一个**视频的音视频流文件。
// 切视频时先 retain_only(新 bvid)，把其余的删干净，再开始下新的。
//
// 为什么不做「保留最近 N 个」的 LRU：
//   手机存储是稀缺资源，一年 B 站视频按 1080P 算一个几百 MB，
//   LRU 只要 N≥3 就能吃掉 1.5GB，而命中率提升几乎为零
//   （用户很少回到上一个看过的视频）。只留当前这一个，
//   同时满足「循环播放不重下」和「占用恒定」两个硬要求。
//
// 目录布局：
//   <cacheDir>/video/v_<bvid>.m4s     视频流
//   <cacheDir>/video/a_<bvid>.m4s     音频流
//   <cacheDir>/covers/<bvid>.img      封面原始字节（webp/jpg，解码在用时做）
//   <filesDir>/feed_cache.json        首页列表（见 feed.rs）
//
// 封面缓存**不**跟着切视频删：它们是几十 KB 的小文件，
// 首页那种上百张的量级也就几 MB，留着才能让下次打开首页秒出图。
// ==============================================================================

use std::path::{Path, PathBuf};

/// 应用缓存根目录（系统空间不足时可以清掉，所以只放可重建的数据）。
pub fn base_dir() -> PathBuf {
    let base = crate::android_window::cache_dir()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/data/data/com.bli.rs/cache"));
    let _ = std::fs::create_dir_all(&base);
    base
}

/// 视频流目录
pub fn video_dir() -> PathBuf {
    let p = base_dir().join("video");
    let _ = std::fs::create_dir_all(&p);
    p
}

/// 封面缓存目录
pub fn cover_dir() -> PathBuf {
    let p = base_dir().join("covers");
    let _ = std::fs::create_dir_all(&p);
    p
}

/// 某条流的落盘路径。`kind` 用 'v' / 'a'。
pub fn stream_path(bvid: &str, kind: char) -> PathBuf {
    video_dir().join(format!("{kind}_{bvid}.m4s"))
}

/// 只保留 `keep_bvid` 的音视频流，其余全部删除。
///
/// 在**开始下载新视频之前**调用：那一刻旧视频的解码器已经被 stop_current()
/// 关掉了，文件句柄不再被持有，删得掉。
///
/// `keep_bvid` 为空表示全清。
pub fn retain_only(keep_bvid: &str) {
    let dir = video_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };

    let keep_v = format!("v_{keep_bvid}.m4s");
    let keep_a = format!("a_{keep_bvid}.m4s");
    let mut freed = 0u64;
    let mut removed = 0usize;

    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name == keep_v || name == keep_a {
            continue;
        }
        if !(name.starts_with('v') || name.starts_with('a')) {
            continue;
        }
        if let Ok(meta) = e.metadata() {
            freed += meta.len();
        }
        if std::fs::remove_file(e.path()).is_ok() {
            removed += 1;
        }
    }

    if removed > 0 {
        crate::mxprobe::log_public(&format!(
            "[Cache] 清理上一个视频的流缓存：{} 个文件，释放 {:.1} MB（保留 {keep_bvid}）",
            removed,
            freed as f64 / 1_048_576.0
        ));
    }
}

/// 清空所有流缓存（不含封面）。
pub fn clear_streams() {
    let _ = std::fs::remove_dir_all(video_dir());
    let _ = std::fs::create_dir_all(video_dir());
}

/// 目录总大小（字节）。设置页将来显示"缓存占用"用。
pub fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .map(|m| if m.is_dir() { 0 } else { m.len() })
        .sum()
}

pub fn total_size() -> u64 {
    dir_size(&video_dir()) + dir_size(&cover_dir())
}

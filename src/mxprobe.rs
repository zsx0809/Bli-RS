// ==============================================================================
// MediaExtractor 可行性探针（临时，验证通过后可删）
//
// 目的：确认 AMediaExtractor 能否在**应用进程内**打开 B 站的 fMP4 分段。
//
// 背景：在 root shell 里直接跑同样的代码，setDataSource 一律返回 -10002
// （连系统自带的普通 mp4 也打不开）。这说明不是 fMP4 的问题，
// 而是 root shell 缺少 media 服务的 binder 上下文。
// 必须在 App 进程里验证才有意义。
// ==============================================================================

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};

#[repr(C)]
struct AMediaExtractor {
    _p: [u8; 0],
}
#[repr(C)]
struct AMediaFormat {
    _p: [u8; 0],
}

#[link(name = "mediandk")]
extern "C" {
    fn AMediaExtractor_new() -> *mut AMediaExtractor;
    fn AMediaExtractor_delete(e: *mut AMediaExtractor) -> c_int;
    fn AMediaExtractor_setDataSource(e: *mut AMediaExtractor, path: *const c_char) -> c_int;
    fn AMediaExtractor_setDataSourceFd(
        e: *mut AMediaExtractor,
        fd: c_int,
        offset: u64,
        length: u64,
    ) -> c_int;
    fn AMediaExtractor_getTrackCount(e: *mut AMediaExtractor) -> usize;
    fn AMediaExtractor_getTrackFormat(e: *mut AMediaExtractor, idx: usize) -> *mut AMediaFormat;
    fn AMediaFormat_delete(f: *mut AMediaFormat) -> c_int;
    fn AMediaFormat_toString(f: *mut AMediaFormat) -> *const c_char;
}

#[link(name = "log")]
extern "C" {
    fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
}

use std::collections::VecDeque;
use std::sync::Mutex;

static LOG_BUFFER: Mutex<Option<VecDeque<String>>> = Mutex::new(None);
const MAX_LOG_LINES: usize = 400;

/// 供其他模块复用的全链路日志出口：落盘 + 内存环形缓存 + logcat
pub fn log_public(msg: &str) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let millis = now.subsec_millis();
    let h = (secs / 3600 + 8) % 24; // 转换东八区北京时间
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    let line = format!("[{h:02}:{m:02}:{s:02}.{millis:03}] {msg}");

    // 1. Android Logcat
    alog(&line);

    // 2. 内存环形缓冲（供 UI 弹窗实时查看）
    if let Ok(mut lock) = LOG_BUFFER.lock() {
        let buf = lock.get_or_insert_with(|| VecDeque::with_capacity(MAX_LOG_LINES));
        if buf.len() >= MAX_LOG_LINES {
            buf.pop_front();
        }
        buf.push_back(line.clone());
    }

    // 3. 文件落盘（追加到 /data/data/com.bli.rs/cache/bli.log）
    append_log_file(&line);
}

fn append_log_file(line: &str) {
    use std::io::Write;
    let log_path = std::path::PathBuf::from("/data/data/com.bli.rs/cache/bli.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
        let _ = writeln!(f, "{line}");
    }
}

/// 获取最近收集的所有日志，供 UI 实时展示
pub fn get_recent_logs() -> String {
    if let Ok(mut lock) = LOG_BUFFER.lock() {
        let buf = lock.get_or_insert_with(|| VecDeque::with_capacity(MAX_LOG_LINES));
        if buf.is_empty() {
            return "暂无日志记录".into();
        }
        buf.iter().cloned().collect::<Vec<_>>().join("\n")
    } else {
        "日志缓冲区忙".into()
    }
}

/// 清空日志
pub fn clear_logs() {
    if let Ok(mut lock) = LOG_BUFFER.lock() {
        if let Some(buf) = lock.as_mut() {
            buf.clear();
        }
    }
    let log_path = std::path::PathBuf::from("/data/data/com.bli.rs/cache/bli.log");
    let _ = std::fs::write(log_path, "");
}

fn alog(msg: &str) {
    if let Ok(c) = CString::new(msg.replace('\0', "")) {
        let tag = CString::new("bli-mx").unwrap();
        unsafe {
            __android_log_write(4 /* INFO */, tag.as_ptr(), c.as_ptr());
        }
    }
}

/// 打开一个媒体文件并打印轨道信息。返回 setDataSource 的原始返回值。
pub fn probe_file(path: &str) -> i32 {
    let Ok(cpath) = CString::new(path) else {
        return -999;
    };

    unsafe {
        let ex = AMediaExtractor_new();
        if ex.is_null() {
            alog("AMediaExtractor_new 失败");
            return -998;
        }

        let r = AMediaExtractor_setDataSource(ex, cpath.as_ptr());
        alog(&format!("setDataSource({path}) -> {r}"));

        if r == 0 {
            let n = AMediaExtractor_getTrackCount(ex);
            alog(&format!("  track_count = {n}"));
            for i in 0..n {
                let f = AMediaExtractor_getTrackFormat(ex, i);
                if f.is_null() {
                    continue;
                }
                let s = AMediaFormat_toString(f);
                if !s.is_null() {
                    alog(&format!("  track {i}: {}", CStr::from_ptr(s).to_string_lossy()));
                }
                AMediaFormat_delete(f);
            }
        }

        AMediaExtractor_delete(ex);
        r
    }
}

/// 用文件描述符打开（绕开基于 JNI 的路径解析）。
///
/// 为什么必须试这条：
///   `setDataSource(path)` 在纯 native 进程里一律返回 -10002，
///   即使文件有读权限、即使是系统自带的普通 mp4。
///   原因是它内部要用 JNI 或 Java FileDescriptor 去做路径解析，
///   我们的线程没有 JNIEnv —— 不是权限问题，是调用路径不对。
///   `setDataSourceFd` 直接吃裸 fd，不需要任何 Java 上下文。
pub fn probe_fd(path: &str) -> i32 {
    use std::os::unix::io::AsRawFd;

    let f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            alog(&format!("打开 {path} 失败: {e}"));
            return -997;
        }
    };
    let len = match f.metadata() {
        Ok(m) => m.len(),
        Err(e) => {
            alog(&format!("取 {path} 长度失败: {e}"));
            return -996;
        }
    };

    unsafe {
        let ex = AMediaExtractor_new();
        if ex.is_null() {
            return -998;
        }
        let r = AMediaExtractor_setDataSourceFd(ex, f.as_raw_fd(), 0, len);
        alog(&format!("setDataSourceFd({path}, len={len}) -> {r}"));

        if r == 0 {
            let n = AMediaExtractor_getTrackCount(ex);
            alog(&format!("  track_count = {n}"));
            for i in 0..n {
                let fmt = AMediaExtractor_getTrackFormat(ex, i);
                if fmt.is_null() {
                    continue;
                }
                let s = AMediaFormat_toString(fmt);
                if !s.is_null() {
                    alog(&format!(
                        "  track {i}: {}",
                        CStr::from_ptr(s).to_string_lossy()
                    ));
                }
                AMediaFormat_delete(fmt);
            }
        }

        AMediaExtractor_delete(ex);
        r
    }
}

/// 在后台线程跑一组测试，不阻塞 UI。
pub fn run_probes() {
    std::thread::spawn(|| {
        alog("=== MediaExtractor 探针开始 ===");

        // 先确认 fd 路径是否可行（这才是我们真正会用的方式）
        alog("--- 用 fd 打开（真正的方案）---");
        probe_fd("/sdcard/v.m4s");
        probe_fd("/data/local/tmp/mx/v.m4s");
        probe_fd("/data/data/com.sinovatech.unicom.ui/files/kaiping_1210.mp4");

        alog("=== 探针结束 ===");
    });
}

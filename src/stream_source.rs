// ==============================================================================
// 边下边播数据源 (AMediaDataSource)
//
// 为什么必须有这个东西：
//   AMediaExtractor 的 setDataSourceFd(fd, 0, len) 在打开那一刻就固定了
//   可读长度 len。如果文件还在下载、只会变长，extractor 读到自己记录的
//   EOF 就返回 -1（输入结束），解码器立即发 EOS —— 画面冻结。
//   文件后来涨到多大，它都再也看不见。
//
//   这就是"播到一半卡住"的根因，也是为什么必须先下完才能播（那方案对
//   一小时的长视频完全不可接受）。
//
// AMediaDataSource 提供了破局点：
//   把数据源从"一个固定长度的 fd"换成一个**回调对象**，解码器每次要数据
//   时调用我们的 readAt(offset, buffer, size)。我们在回调里：
//     · 数据已在缓冲里  → 直接拷给它
//     · 数据还没下到    → 阻塞等待下载线程推进（最多等 timeout）
//     · 文件已下完且越界 → 返回 0，表示真正的 EOF
//   于是解码器可以"跟着下载进度走"，下载慢它就等，下载快它就追。
//
// 三个回调的分工（缺一不可）：
//   readAt(offset, buf, size)  读数据；返回读到的字节数，0=EOF，<0=错误
//   getSize()                  总长度；**下载中返回 -1**（未知），
//                              下完返回真实长度
//   getAvailableSize(offset)   从 offset 起已可读多少字节；解码器用它
//                              判断能不能 seek 到某个位置
//
// getSize 返回 -1 是官方的"长度未知、数据流式到达"约定，媒体框架会据此
// 采用流式解析策略，而不是一开始就要求完整 moov。
// ==============================================================================

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// 下载进度与缓冲。下载线程写，解码线程读。
pub struct SharedBuffer {
    inner: Mutex<BufState>,
    /// 有新数据 / 下载结束时通知等待者
    cv: Condvar,
}

struct BufState {
    /// 已知的最终总长度（来自 HTTP Content-Length）。
    ///
    /// 为什么必须提前知道：AMediaExtractor 在解封装阶段调用 getSize()，
    /// 若返回 -1（"长度未知"），实测它会**一直不返回**，整个解码线程卡死
    /// 在 setDataSourceCustom 里。所以必须先拿到真实总长度。
    known_total: Option<u64>,
    /// 已落盘并可供读取的字节数
    available: u64,
    /// 下载是否已完成（此时 available 就是最终长度）
    finished: bool,
    /// 下载是否失败（失败后 readAt 返回错误而不是一直等）
    failed: bool,
    /// 最终总长度（finished 后有效；未知时为 None）
    total: Option<u64>,
    /// 落盘文件路径
    path: std::path::PathBuf,
}

impl SharedBuffer {
    /// `known_total` 是 HTTP Content-Length；必须先向 CDN 问出来。
    pub fn new(path: std::path::PathBuf, known_total: Option<u64>) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(BufState {
                known_total,
                available: 0,
                finished: false,
                failed: false,
                total: None,
                path,
            }),
            cv: Condvar::new(),
        })
    }

    /// 当前已可读字节数（下载线程用于计算增量）
    pub fn available_now(&self) -> u64 {
        match self.inner.lock() {
            Ok(g) => g.available,
            Err(e) => e.into_inner().available,
        }
    }

    /// 下载线程调用：追加了 n 字节
    pub fn advance(&self, n: u64) {
        if let Ok(mut s) = self.inner.lock() {
            s.available += n;
        }
        self.cv.notify_all();
    }

    /// 下载线程调用：完成，total 是最终长度
    pub fn finish(&self, total: u64) {
        if let Ok(mut s) = self.inner.lock() {
            s.available = s.available.max(total);
            s.total = Some(total);
            s.finished = true;
        }
        self.cv.notify_all();
    }

    /// 下载线程调用：失败
    pub fn fail(&self) {
        if let Ok(mut s) = self.inner.lock() {
            s.failed = true;
        }
        self.cv.notify_all();
    }

    /// 解码线程用：等 offset 起至少有 size 字节可读（或下载结束/失败）。
    ///
    /// 返回 (可用字节数, 是否已结束, 是否失败)
    fn wait_for(&self, offset: u64, size: usize) -> (u64, bool, bool) {
        let need = offset + size as u64;
        let mut s = match self.inner.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        loop {
            // 以**实际文件大小**为准，而不是下载线程上报的计数。
            //
            // 为什么：下载线程是按 chunk 写入的，但 reqwest 的 chunk 边界
            // 和"已 flush 到磁盘"的时刻并不一致；而且续传重试时会有
            // 多个写入阶段。直接 stat 文件既简单又不会漏。
            // 这条也自动解决了"下载线程还没来得及 advance"的时序问题。
            if let Ok(meta) = std::fs::metadata(&s.path) {
                let real = meta.len();
                if real > s.available {
                    s.available = real;
                }
            }

            if s.failed {
                return (0, true, true);
            }
            if s.available >= need {
                return (s.available, false, false);
            }
            if s.finished {
                // 下完了但不够 —— 这是真正的 EOF
                return (s.available, true, false);
            }
            // 数据还没到，等下载线程推一把。
            // 用 wait_timeout 而不是 wait：防止下载线程意外死掉时永久挂起。
            let (guard, timeout) = match self.cv.wait_timeout(s, std::time::Duration::from_millis(200)) {
                Ok(v) => v,
                Err(e) => e.into_inner(),
            };
            s = guard;
            if timeout.timed_out() && s.available < need && !s.finished {
                // 超时一次不算失败，继续循环等 —— 弱网下大块读取可能很慢
                continue;
            }
        }
    }
}

/// 传给 C 回调的上下文。必须 Box::leak 后交给 AMediaDataSource_setUserdata，
/// 由 close 回调负责回收。
struct SourceCtx {
    buf: Arc<SharedBuffer>,
}

/// 每个数据源实例独立的 fd（readAt 内部定位读）
///
/// 为什么不用 File::read_at（pread）：
///   标准库的 FileExt::read_at 在 Android 上可用，且是原子的
///   （不共享文件游标），正好适配"解码线程随时按 offset 读"的模式。
unsafe extern "C" fn ds_read_at(
    userdata: *mut std::ffi::c_void,
    offset: i64,
    buffer: *mut std::ffi::c_void,
    size: usize,
) -> isize {
    use std::os::unix::fs::FileExt;

    if userdata.is_null() || buffer.is_null() {
        return -1;
    }
    let ctx = &*(userdata as *const SourceCtx);
    let offset = offset.max(0) as u64;

    let (avail, finished, failed) = ctx.buf.wait_for(offset, size);
    if failed {
        // 下载失败：返回 0（EOF）而不是 -1（错误）。
        //
        // 为什么：返回 -1 会让 AMediaExtractor 立即判定整个数据源不可用，
        // 直接把 setDataSourceCustom 打成失败(-10000)，连已经解出来的
        // 部分都保不住。返回 0 至少让解码器把手上已有的数据播完再结束，
        // 比硬失败体验好。
        return 0;
    }
    if offset >= avail {
        // 没有更多数据：下完了就是 EOF，没下完就是还没到（返回 0 让框架稍后再来）
        return 0;
    }

    let path = {
        match ctx.buf.inner.lock() {
            Ok(s) => s.path.clone(),
            Err(e) => e.into_inner().path.clone(),
        }
    };

    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return -1,
    };

    // 只读到"确实已写入"的部分，绝不越过 available 边界去读空洞
    let to_read = std::cmp::min(size as u64, avail - offset) as usize;
    if to_read == 0 {
        return if finished { 0 } else { 0 };
    }

    let slice = std::slice::from_raw_parts_mut(buffer as *mut u8, to_read);
    match file.read_at(slice, offset) {
        Ok(n) => n as isize,
        Err(_) => -1,
    }
}

unsafe extern "C" fn ds_get_size(userdata: *mut std::ffi::c_void) -> isize {
    if userdata.is_null() {
        return -1;
    }
    let ctx = &*(userdata as *const SourceCtx);
    let s = match ctx.buf.inner.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    let result = match s.known_total.or(s.total) {
        Some(t) => t as isize,
        None => 0,
    };
    result
}

unsafe extern "C" fn ds_get_available_size(
    userdata: *mut std::ffi::c_void,
    offset: i64,
) -> isize {
    if userdata.is_null() {
        return 0;
    }
    let ctx = &*(userdata as *const SourceCtx);
    let off = offset.max(0) as u64;

    // 关键：这里也要**等**数据到达，不能立刻返回 0。
    //
    // 为什么：媒体框架在解封装阶段用 getAvailableSize 判断
    // "从 offset 起有多少字节能读"。如果文件刚起播、数据还没写到
    // offset 处，立刻返回 0 会让解析器认为那里什么都没有，
    // 于是放弃解析 —— 表现为 setDataSourceCustom 长时间不返回（卡住），
    // 或者直接失败。
    //
    // 正确做法与 readAt 一致：等一小段数据出来再回报。
    // 等待上限 5 秒：真没数据（下载挂了）时也要能返回，不能死等。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        {
            let mut s = match ctx.buf.inner.lock() {
                Ok(g) => g,
                Err(e) => e.into_inner(),
            };
            // 同样以实际文件大小为准
            if let Ok(meta) = std::fs::metadata(&s.path) {
                let real = meta.len();
                if real > s.available {
                    s.available = real;
                }
            }
            if s.available > off {
                return (s.available - off) as isize;
            }
            // 下载已结束且没有更多数据 —— 就该返回 0（真的到头了）
            if s.finished || s.failed {
                return 0;
            }
        }
        if std::time::Instant::now() >= deadline {
            return 0;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

unsafe extern "C" fn ds_close(userdata: *mut std::ffi::c_void) {
    if userdata.is_null() {
        return;
    }
    // 回收 read_at 里 Box::leak 出去的 SourceCtx
    drop(Box::from_raw(userdata as *mut SourceCtx));
}

/// 创建一个绑定到 `buf` 的 AMediaDataSource。
///
/// 返回裸指针，由调用方交给 AMediaExtractor_setDataSource 并在不用时
/// AMediaDataSource_delete。生命周期由 extractor 与这里共同保证。
pub fn create_data_source(buf: Arc<SharedBuffer>) -> *mut ndk_sys::AMediaDataSource {
    unsafe {
        let ds = ndk_sys::AMediaDataSource_new();
        if ds.is_null() {
            return std::ptr::null_mut();
        }

        let ctx = Box::new(SourceCtx { buf });
        let userdata = Box::into_raw(ctx) as *mut std::ffi::c_void;

        ndk_sys::AMediaDataSource_setUserdata(ds, userdata);
        ndk_sys::AMediaDataSource_setReadAt(ds, Some(ds_read_at));
        ndk_sys::AMediaDataSource_setGetSize(ds, Some(ds_get_size));
        ndk_sys::AMediaDataSource_setGetAvailableSize(ds, Some(ds_get_available_size));
        ndk_sys::AMediaDataSource_setClose(ds, Some(ds_close));

        ds
    }
}

// 未使用但保留：提示后续按需扩展（比如给 framework 提供更精确的 buffer 状态）
#[allow(dead_code)]
static _UNUSED: AtomicBool = AtomicBool::new(false);
#[allow(dead_code)]
static _UNUSED2: AtomicU64 = AtomicU64::new(0);

// ==============================================================================
// 视频解码器
//
// 链路：
//   AMediaExtractor(fd) 读样本
//        ↓ 写入 MediaCodec 输入缓冲
//   MediaCodec(configure 到 Surface) 硬解
//        ↓ releaseOutputBuffer(render=true)
//   SurfaceView 的 Surface  →  SurfaceFlinger 合成 → 屏幕
//
// 为什么全程零拷贝：
//   MediaCodec 配置时直接绑定 Surface，解码器把 YUV 转成 GPU 纹理，
//   直接提交给合成器。CPU 侧只搬运压缩后的码流（几百 KB/s），
//   不做任何像素拷贝。1080p30 的解码 + 上屏功耗因此可以压到很低。
//
// 线程模型：
//   解码在独立线程跑，循环 dequeue/queue。
//   停止通过 Arc<AtomicBool> 通知，不用 join 等待（避免卡 UI）。
// ==============================================================================

use std::os::raw::c_int;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ndk::media::media_codec::{
    DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection,
};
use ndk::media::media_format::MediaFormat;
use ndk::native_window::NativeWindow;

/// AMediaExtractor 的最小 FFI 封装。
///
/// 为什么不用现成 crate：
///   - ndk / ndk-sys 都没有 AMediaExtractor 的封装（查过，ndk-sys 里
///     只有 AMediaCodec 相关）。
///   - rust_mediacodec 那个 crate 有，但它无 license、2022 年后停更、12 星，
///     不适合作为依赖。所以自己写这几十行。
#[allow(dead_code)]
mod extractor {
    use std::os::raw::{c_char, c_int};

    #[repr(C)]
    pub struct AMediaExtractor {
        _p: [u8; 0],
    }
    #[repr(C)]
    pub struct AMediaFormat {
        _p: [u8; 0],
    }

    #[link(name = "mediandk")]
    extern "C" {
        pub fn AMediaExtractor_new() -> *mut AMediaExtractor;
        pub fn AMediaExtractor_delete(e: *mut AMediaExtractor) -> c_int;

        /// 用文件描述符设置数据源。
        ///
        /// **不要用 AMediaExtractor_setDataSource（字符串路径）** ——
        /// 它内部依赖 JNI 做路径解析，纯 native 线程没有 JNIEnv，
        /// 一律返回 -10002（UNKNOWN_ERROR）。实测连系统自带的普通 mp4
        /// 都打不开，与权限、文件格式都无关。
        pub fn AMediaExtractor_setDataSourceFd(
            e: *mut AMediaExtractor,
            fd: c_int,
            offset: u64,
            length: u64,
        ) -> c_int;

        pub fn AMediaExtractor_getTrackCount(e: *mut AMediaExtractor) -> usize;
        pub fn AMediaExtractor_getTrackFormat(
            e: *mut AMediaExtractor,
            idx: usize,
        ) -> *mut AMediaFormat;
        pub fn AMediaExtractor_selectTrack(e: *mut AMediaExtractor, idx: usize) -> c_int;
        pub fn AMediaExtractor_seekTo(e: *mut AMediaExtractor, time: i64, mode: c_int) -> c_int;
        pub fn AMediaExtractor_readSampleData(
            e: *mut AMediaExtractor,
            buf: *mut u8,
            cap: usize,
        ) -> isize;
        pub fn AMediaExtractor_getSampleTime(e: *mut AMediaExtractor) -> i64;
        pub fn AMediaExtractor_getSampleTrackIndex(e: *mut AMediaExtractor) -> c_int;
        pub fn AMediaExtractor_advance(e: *mut AMediaExtractor) -> c_int;

        pub fn AMediaFormat_delete(f: *mut AMediaFormat) -> c_int;
        pub fn AMediaFormat_toString(f: *mut AMediaFormat) -> *const c_char;
    }
}

pub use extractor::AMediaExtractor;

/// 把裸指针包成可跨线程传递的类型。
///
/// `*mut c_void` 不是 Send，但 Surface 指针的生命周期由 Java 侧的
/// SurfaceView 持有，跨线程只读使用是安全的（MediaCodec 内部有同步）。
struct SendPtr(*mut std::ffi::c_void);
unsafe impl Send for SendPtr {}

impl SendPtr {
    /// 取出裸指针。
    ///
    /// 走方法而不是直接读字段：Rust 2021 的闭包按字段捕获，
    /// `sp.0` 会让闭包只捕获裸指针本身，Send 包装因此失效。
    /// 通过方法访问则整个 `SendPtr` 被捕获，Send 约束成立。
    fn get(&self) -> *mut std::ffi::c_void {
        self.0
    }
}

/// 视频解码器。持有 extractor、codec 与停止标志。
pub struct VideoDecoder {
    stop: Arc<AtomicBool>,
}

impl VideoDecoder {
    /// 用本地文件启动解码，输送到 `surface`。
    ///
    /// `path` 是已经下载好的 fMP4 分段（B 站的 .m4s）。
    /// 返回后解码在后台线程进行，`stop()` 可停止。
    pub fn start(
        path: String,
        surface: *mut std::ffi::c_void,
        on_first_frame: impl FnOnce() + Send + 'static,
        on_error: impl Fn(String) + Send + 'static,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = stop.clone();
        let sp = SendPtr(surface);

        std::thread::spawn(move || {
            // 在闭包内（已成为新线程的栈）才解包指针。
            // 若在外部 `sp.0` 提前取出，裸指针会被捕获进闭包，
            // 编译期就会因为 *mut c_void 不是 Send 而报错。
            let surface_ptr = sp.get();
            if let Err(e) = decode_loop(&path, surface_ptr, on_first_frame, &stop_for_thread) {
                on_error(e);
            }
        });

        Self { stop }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// 解码主循环
fn decode_loop(
    path: &str,
    surface: *mut std::ffi::c_void,
    on_first_frame: impl FnOnce(),
    stop: &AtomicBool,
) -> Result<(), String> {
    use std::os::unix::io::AsRawFd;

    let file = std::fs::File::open(path).map_err(|e| format!("打开 {path} 失败: {e}"))?;
    let len = file
        .metadata()
        .map_err(|e| format!("取文件长度失败: {e}"))?
        .len();

    unsafe {
        // ---- 1. 解封装 ----
        let ex = extractor::AMediaExtractor_new();
        if ex.is_null() {
            return Err("AMediaExtractor_new 失败".into());
        }

        let r = extractor::AMediaExtractor_setDataSourceFd(ex, file.as_raw_fd(), 0, len);
        if r != 0 {
            extractor::AMediaExtractor_delete(ex);
            return Err(format!("setDataSourceFd 失败: {r}"));
        }

        let tracks = extractor::AMediaExtractor_getTrackCount(ex);
        if tracks == 0 {
            extractor::AMediaExtractor_delete(ex);
            return Err("文件里没有轨道".into());
        }

        // 选第一条视频轨
        let mut video_idx = None;
        let mut format = MediaFormat::new();

        for i in 0..tracks {
            let fmt = extractor::AMediaExtractor_getTrackFormat(ex, i);
            if fmt.is_null() {
                continue;
            }
            let s = extractor::AMediaFormat_toString(fmt);
            let desc = if s.is_null() {
                String::new()
            } else {
                std::ffi::CStr::from_ptr(s).to_string_lossy().to_string()
            };

            if desc.contains("video/") {
                video_idx = Some(i);
                // 把 AMediaFormat 转成 ndk 的 MediaFormat。
                // ndk 的 MediaFormat::from_ptr 是 unsafe 且要求所有权转移，
                // 这里我们复制一份再用，原对象照常 delete。
                // from_ptr 接管所有权（要求 NonNull），所以之后不能再 delete。
                // 我们的 AMediaFormat 是自声明的不透明类型，转成 ndk_sys 的即可。
                if let Some(nn) = std::ptr::NonNull::new(fmt as *mut ndk_sys::AMediaFormat) {
                    format = MediaFormat::from_ptr(nn);
                }
                break;
            }
            extractor::AMediaFormat_delete(fmt);
        }

        let Some(vidx) = video_idx else {
            extractor::AMediaExtractor_delete(ex);
            return Err("没有找到视频轨".into());
        };

        extractor::AMediaExtractor_selectTrack(ex, vidx);

        // 取 mime 建解码器
        let mime = format
            .str("mime")
            .map(|s| s.to_string())
            .unwrap_or_else(|| "video/avc".into());

        // ---- 2. 建解码器 ----
        let Some(codec) = MediaCodec::from_decoder_type(&mime) else {
            extractor::AMediaExtractor_delete(ex);
            return Err(format!("找不到 {mime} 的解码器"));
        };

        // ---- 3. 配置到 Surface ----
        // 我们自己声明的 ANativeWindow 是不透明结构体，
        // 这里要转成 ndk_sys 的类型。二者都是 ABI 兼容的裸指针，
        // 直接 cast 即可。
        let win = NativeWindow::from_ptr(
            std::ptr::NonNull::new(surface as *mut ndk_sys::ANativeWindow)
                .ok_or("Surface 指针为空")?,
        );

        codec
            .configure(&format, Some(&win), MediaCodecDirection::Decoder)
            .map_err(|e| format!("configure 失败: {e:?}"))?;

        codec.start().map_err(|e| format!("start 失败: {e:?}"))?;

        // ---- 4. 解码循环 ----
        let mut input_done = false;
        let mut output_done = false;
        let timeout = Duration::from_millis(8);

        // PTS 帧同步时钟，杜绝硬件极限无等待刷帧导致的“快进/倍速播放”
        let mut first_pts: Option<i64> = None;
        let mut start_instant: Option<std::time::Instant> = None;
        let mut first_frame_sent = false;
        let mut on_first_frame = Some(on_first_frame);

        while !output_done && !stop.load(Ordering::Relaxed) {
            // 送输入
            if !input_done {
                match codec.dequeue_input_buffer(timeout) {
                    Ok(DequeuedInputBufferResult::Buffer(mut buf)) => {
                        let cap = buf.buffer_mut().len();
                        let n = extractor::AMediaExtractor_readSampleData(
                            ex,
                            buf.buffer_mut().as_mut_ptr() as *mut u8,
                            cap,
                        );

                        if n < 0 {
                            // 输入结束
                            codec.set_signal_end_of_input_stream().ok();
                            input_done = true;
                        } else {
                            let pts = extractor::AMediaExtractor_getSampleTime(ex);
                            // 注意：AMediaExtractor_getSampleTime 返回的是微秒 (us)，绝不能再乘以 1000
                            let pts_us = if pts >= 0 { pts as u64 } else { 0 };
                            codec
                                .queue_input_buffer(
                                    buf,
                                    0,
                                    n as usize,
                                    pts_us,
                                    0,
                                )
                                .ok();
                            extractor::AMediaExtractor_advance(ex);
                        }
                    }
                    Ok(DequeuedInputBufferResult::TryAgainLater) => {}
                    Err(e) => return Err(format!("dequeue_input 失败: {e:?}")),
                }
            }

            // 取输出
            match codec.dequeue_output_buffer(timeout) {
                Ok(DequeuedOutputBufferInfoResult::Buffer(buf)) => {
                    // AMediaCodec 的 buffer 标志常量。
                    // ndk crate 没有导出它，但 AOSP 里定义为 1<<2。
                    const BUFFER_FLAG_END_OF_STREAM: u32 = 4;

                    let eos = buf.info().flags() & BUFFER_FLAG_END_OF_STREAM != 0;
                    let pts_us = buf.info().presentation_time_us();

                    // PTS 帧同步：根据视频实际时间戳（微秒）休眠，实现恒定 1.0x 正常速度播放
                    if pts_us >= 0 {
                        if first_pts.is_none() {
                            first_pts = Some(pts_us);
                            start_instant = Some(std::time::Instant::now());
                        } else if let (Some(base_pts), Some(start_t)) = (first_pts, start_instant) {
                            let target_elapsed_us = pts_us - base_pts;
                            let actual_elapsed_us = start_t.elapsed().as_micros() as i64;
                            let diff_us = target_elapsed_us - actual_elapsed_us;

                            if diff_us > 0 {
                                if diff_us <= 80_000 {
                                    // 正常帧间隔等待（最多休眠 80ms，杜绝任何假死卡顿）
                                    std::thread::sleep(Duration::from_micros(diff_us as u64));
                                } else {
                                    // 差值过大说明视频流发生时间戳跳变或切片断层，重新校准基准时基
                                    first_pts = Some(pts_us);
                                    start_instant = Some(std::time::Instant::now());
                                }
                            } else if diff_us < -150_000 {
                                // 解码落后超过 150ms，重设时钟追赶，杜绝持续慢放
                                first_pts = Some(pts_us);
                                start_instant = Some(std::time::Instant::now());
                            }
                        }
                    }

                    // render=true → 交给 Surface 显示。
                    codec.release_output_buffer(buf, true).ok();

                    // 首帧已送达 SurfaceView，通知 UI 淡出封面占位图
                    if !first_frame_sent {
                        first_frame_sent = true;
                        if let Some(cb) = on_first_frame.take() {
                            cb();
                        }
                    }

                    if eos {
                        output_done = true;
                    }
                }
                Ok(DequeuedOutputBufferInfoResult::OutputFormatChanged) => {
                    // 输出格式变化（首帧常见），继续即可
                }
                Ok(DequeuedOutputBufferInfoResult::OutputBuffersChanged) => {}
                Ok(DequeuedOutputBufferInfoResult::TryAgainLater) => {}
                Err(e) => return Err(format!("dequeue_output 失败: {e:?}")),
            }
        }

        codec.stop().ok();
        extractor::AMediaExtractor_delete(ex);
    }

    Ok(())
}

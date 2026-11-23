// ==============================================================================
// 音频解码器 (MediaCodec → PCM)
//
// 与视频解码的区别：
//   · 视频**配置到 Surface**，解码结果直接进合成器（零拷贝）
//   · 音频**不配置 Surface**，要自己读输出缓冲区拿 PCM，再喂给 AAudio
//
// 线程模型：
//   独立的解码线程，循环 dequeue/queue，把 PCM 写进 AudioOutput 的环形缓冲。
//   写不下就小睡一会 —— 这是背压，不是错误（说明播放端还没消费完）。
//
// 音画同步策略（关键）：
//   **不做**任何"音画对齐"的主动干预。因为：
//     · 音频由 AAudio 硬件时钟驱动，它自己是绝对稳定的
//     · 视频侧每帧先检查 PTS 与 audio_clock.position_us() 的差，
//       落后就立刻送显（追上去），超前就等
//   所以同步逻辑在**视频侧**，音频只管按时喂数据。
//
// 三条曾经导致「音画不同步」的坑（已修，别改回去）
// ------------------------------------------------------------------------------
// 1. **采样率必须用解码器实际输出的，不能用固定的 48k。**
//    B 站 DASH 的 AAC 有 44100 和 48000 两种。按 48k 打开 AAudio 去播
//    44.1k 的数据，声音会快 48000/44100 = 1.088 倍，一分钟偏出 5 秒。
//    症状正是「有的视频同步、有的极度不同步」—— 因为源本身不固定。
//    现在从 MediaFormat 的 sample-rate 读，通知输出侧重建时钟格式。
//
// 2. **seek 必须同时作用于音频。**
//    早先 seek 只对视频 extractor 做，音频还在原地播，
//    跳完之后画面和声音差了"跳转距离"那么多 —— 差得极其夸张。
//
// 3. **音频 seek 后要重新以目标 PTS 为基准，并清掉环形缓冲。**
//    见 AudioOutput::seek_to。
// ==============================================================================

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use ndk::media::media_codec::{
    DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection,
};
use ndk::media::media_format::MediaFormat;

use crate::audio_out::AudioOutput;

/// 音频解码器句柄
pub struct AudioDecoder {
    stop: Arc<AtomicBool>,
    /// 待执行的 seek 目标（微秒）。与视频侧的 Controls::seek_target_us 同构。
    seek_target_us: Arc<AtomicI64>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl AudioDecoder {
    /// 启动音频解码。
    ///
    /// `source` 是音频 m4s 的流式数据源（与视频同一套 SharedBuffer 机制）。
    /// 返回后解码在后台线程进行。
    pub fn start(
        buffer: Arc<crate::stream_source::SharedBuffer>,
        output: Arc<AudioOutput>,
        on_error: impl Fn(String) + Send + 'static,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = stop.clone();
        let seek = Arc::new(AtomicI64::new(-1));
        let seek_for_thread = seek.clone();

        let handle = std::thread::spawn(move || {
            if let Err(e) = decode_loop(buffer, output, &stop_for_thread, &seek_for_thread) {
                on_error(e);
            }
        });

        Self {
            stop,
            seek_target_us: seek,
            handle: Some(handle),
        }
    }

    /// 请求跳到某个 PTS（微秒）。下一次解码循环迭代会执行。
    pub fn request_seek(&self, pts_us: i64) {
        self.seek_target_us.store(pts_us.max(0), Ordering::SeqCst);
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            std::thread::spawn(move || {
                let _ = h.join();
            });
        }
    }
}

/// 探测音频轨的真实格式：(采样率, 声道数)。
///
/// 为什么必须在**打开 AAudio 流之前**探测：
///   MediaCodec 解出的 PCM 是音轨的原生采样率。AAudio 流一旦按某个
///   采样率打开，它就把收到的字节**按那个速率**播出去，不做重采样。
///   按 48000 打开却喂 44100 的数据 → 声音快 8.8%、音调偏高，
///   而且消费速度与生产速度不匹配，会周期性欠载 —— 听感就是"杂音"。
///
/// 之前这里偷懒固定按 48k 开流，注释里写"AAudio 会自动重采样"，
/// 那是错的。代价是一部分视频音画不同步 + 全程杂音，且症状随机
/// （取决于那个视频的音轨恰好是 44.1k 还是 48k），很难归因。
///
/// 探测本身很便宜：只解封装、读轨道格式，不解码任何帧。
/// 数据源是流式的，但这个函数只碰文件头（moov），首段就够。
pub fn probe_format(buffer: Arc<crate::stream_source::SharedBuffer>) -> (i32, i32) {
    const DEFAULT: (i32, i32) = (48_000, 2);
    unsafe {
        let ex = crate::video_decoder::raw_extractor::AMediaExtractor_new();
        if ex.is_null() {
            return DEFAULT;
        }
        let ds = crate::stream_source::create_data_source(buffer);
        if ds.is_null() {
            crate::video_decoder::raw_extractor::AMediaExtractor_delete(ex);
            return DEFAULT;
        }
        let r = crate::video_decoder::raw_extractor::AMediaExtractor_setDataSourceCustom(ex, ds);
        if r != 0 {
            crate::video_decoder::raw_extractor::AMediaExtractor_delete(ex);
            return DEFAULT;
        }

        let tracks = crate::video_decoder::raw_extractor::AMediaExtractor_getTrackCount(ex);
        let mut out = DEFAULT;
        for i in 0..tracks {
            let fmt = crate::video_decoder::raw_extractor::AMediaExtractor_getTrackFormat(ex, i);
            if fmt.is_null() {
                continue;
            }
            let s = crate::video_decoder::raw_extractor::AMediaFormat_toString(fmt);
            let desc = if s.is_null() {
                String::new()
            } else {
                std::ffi::CStr::from_ptr(s).to_string_lossy().to_string()
            };
            if desc.contains("audio/") {
                if let Some(nn) = std::ptr::NonNull::new(fmt as *mut ndk_sys::AMediaFormat) {
                    let mut f = MediaFormat::from_ptr(nn);
                    out = (
                        f.i32("sample-rate").unwrap_or(DEFAULT.0),
                        f.i32("channel-count").unwrap_or(DEFAULT.1),
                    );
                }
                break;
            }
            crate::video_decoder::raw_extractor::AMediaFormat_delete(fmt);
        }

        // extractor 与 datasource 都要释放。
        //
        // AMediaExtractor_delete 只会通知 datasource 走 close 回调，
        // 不会自己 delete 它 —— 少这一句就是一个 AMediaDataSource 泄漏，
        // 并且那段 Box::leak 出去的 SourceCtx 也回收不回来。
        crate::video_decoder::raw_extractor::AMediaExtractor_delete(ex);
        unsafe { ndk_sys::AMediaDataSource_delete(ds) };
        if out.0 <= 0 || out.1 <= 0 {
            DEFAULT
        } else {
            out
        }
    }
}

fn decode_loop(
    buffer: Arc<crate::stream_source::SharedBuffer>,
    output: Arc<AudioOutput>,
    stop: &AtomicBool,
    seek_target: &AtomicI64,
) -> Result<(), String> {
    unsafe {
        // ---- 1. 解封装 ----
        let ex = crate::video_decoder::raw_extractor::AMediaExtractor_new();
        if ex.is_null() {
            return Err("音频 AMediaExtractor_new 失败".into());
        }

        let ds = crate::stream_source::create_data_source(buffer);
        if ds.is_null() {
            crate::video_decoder::raw_extractor::AMediaExtractor_delete(ex);
            return Err("音频创建 AMediaDataSource 失败".into());
        }

        let r = crate::video_decoder::raw_extractor::AMediaExtractor_setDataSourceCustom(ex, ds);
        if r != 0 {
            crate::video_decoder::raw_extractor::AMediaExtractor_delete(ex);
            return Err(format!("音频 setDataSourceCustom 失败: {r}"));
        }

        // ---- 2. 找音频轨 ----
        let tracks = crate::video_decoder::raw_extractor::AMediaExtractor_getTrackCount(ex);
        let mut audio_idx = None;
        let mut format = MediaFormat::new();

        for i in 0..tracks {
            let fmt = crate::video_decoder::raw_extractor::AMediaExtractor_getTrackFormat(ex, i);
            if fmt.is_null() {
                continue;
            }
            let s = crate::video_decoder::raw_extractor::AMediaFormat_toString(fmt);
            let desc = if s.is_null() {
                String::new()
            } else {
                std::ffi::CStr::from_ptr(s).to_string_lossy().to_string()
            };
            if desc.contains("audio/") {
                audio_idx = Some(i);
                if let Some(nn) = std::ptr::NonNull::new(fmt as *mut ndk_sys::AMediaFormat) {
                    format = MediaFormat::from_ptr(nn);
                }
                break;
            }
            crate::video_decoder::raw_extractor::AMediaFormat_delete(fmt);
        }

        let Some(aidx) = audio_idx else {
            crate::video_decoder::raw_extractor::AMediaExtractor_delete(ex);
            return Err("没有找到音频轨".into());
        };
        crate::video_decoder::raw_extractor::AMediaExtractor_selectTrack(ex, aidx);

        // ---- 2.5 读真实采样率与声道数 ----
        //
        // 这两个值必须来自轨道格式，不能假定 48k/2ch。
        // B 站 DASH 音轨有 44100 和 48000 两种采样率，
        // 按错误的采样率驱动时钟会整体变速（见文件头注释第 1 条）。
        //
        // 注意用上面循环里已经取到的 `format`，不要重复调
        // getTrackFormat —— 那会再 new 一个 AMediaFormat 出来，
        // 既不 delete 也不好跟踪生命周期。
        //
        // MediaFormat::str() 需要 &mut（ndk 的实现签名如此）。
        let src_sample_rate = format.i32("sample-rate").unwrap_or(48000);
        let src_channels = format.i32("channel-count").unwrap_or(2);

        let mime = format
            .str("mime")
            .map(|s| s.to_string())
            .unwrap_or_else(|| "audio/mp4a-latm".into());

        crate::mxprobe::log_public(&format!(
            "[Audio] 轨道格式 mime={mime} sample-rate={src_sample_rate} channels={src_channels}"
        ));

        // 通知输出侧：真实格式。
        //
        // AAudio 流是按 48k/2ch 打开的（见 player.rs 的说明：流一旦打开
        // 就不能改采样率，要重建整个流）。AAudio 自己会做 44.1k→48k 的
        // 重采样，所以**播放不会变速**；需要修正的是我们自己的时钟换算 ——
        // position_us() 若按 48k 换算 44.1k 的实际帧数，算出来的时间会偏快。
        if src_sample_rate > 0 && src_channels > 0 {
            output
                .clock
                .set_format(src_sample_rate as u32, src_channels as u32, 2);
        }

        // ---- 3. 建解码器（不配置 Surface，输出 PCM）----
        let Some(codec) = MediaCodec::from_decoder_type(&mime) else {
            crate::video_decoder::raw_extractor::AMediaExtractor_delete(ex);
            return Err(format!("找不到 {mime} 音频解码器"));
        };

        codec
            .configure(&format, None, MediaCodecDirection::Decoder)
            .map_err(|e| format!("音频 configure 失败: {e:?}"))?;
        codec.start().map_err(|e| format!("音频 start 失败: {e:?}"))?;

        crate::mxprobe::log_public(&format!("[Audio] 解码器就绪 mime={mime}"));

        // ---- 4. 解码循环 ----
        let mut input_done = false;
        let mut output_done = false;
        let timeout = Duration::from_millis(8);
        let mut first_pts: Option<i64> = None;
        // seek 后要丢弃所有 PTS 早于目标的 PCM 块
        let mut seek_drop_until_us: Option<i64> = None;

        while !output_done && !stop.load(Ordering::Relaxed) {
            // ---- 外部控制：seek ----
            //
            // 音频侧必须响应 seek，否则跳转之后画面到了新位置、声音还在
            // 老位置播 —— 差出"跳了多少"，这是最刺眼的一种不同步。
            //
            // AMediaExtractor_seekTo 对音频同样只能落到最近的关键帧（AAC
            // 通常每帧都是同步帧，所以实际上相当精确），落点之前的
            // PCM 块仍然要丢掉，否则会先听到一小段跳转前的声音。
            if let Some(target_us) = {
                let v = seek_target.swap(-1, Ordering::SeqCst);
                if v >= 0 { Some(v) } else { None }
            } {
                const SEEK_TO_CLOSEST_SYNC: i32 = 2;
                let r = crate::video_decoder::raw_extractor::AMediaExtractor_seekTo(
                    ex, target_us, SEEK_TO_CLOSEST_SYNC,
                );
                crate::mxprobe::log_public(&format!(
                    "[Audio][Seek] extractor.seekTo({target_us}us) -> {r}"
                ));
                if r == 0 {
                    // 解码器内部还有旧参考帧，flush 掉
                    codec.flush().ok();
                    input_done = false;
                    first_pts = None;
                    seek_drop_until_us = Some(target_us);
                    // 清空环形缓冲里 seek 之前的旧 PCM，并把时钟挪到目标点
                    output.seek_to(target_us);
                }
            }

            // 送输入
            if !input_done && output.buffered() < output.capacity_bytes() / 2 {
                match codec.dequeue_input_buffer(timeout) {
                    Ok(DequeuedInputBufferResult::Buffer(mut buf)) => {
                        let cap = buf.buffer_mut().len();
                        let n = crate::video_decoder::raw_extractor::AMediaExtractor_readSampleData(
                            ex,
                            buf.buffer_mut().as_mut_ptr() as *mut u8,
                            cap,
                        );

                        if n < 0 {
                            const BUFFER_FLAG_END_OF_STREAM: u32 = 4;
                            codec
                                .queue_input_buffer(buf, 0, 0, 0, BUFFER_FLAG_END_OF_STREAM)
                                .ok();
                            input_done = true;
                        } else {
                            let pts =
                                crate::video_decoder::raw_extractor::AMediaExtractor_getSampleTime(ex);
                            let pts_us = if pts >= 0 { pts as u64 } else { 0 };
                            codec.queue_input_buffer(buf, 0, n as usize, pts_us, 0).ok();
                            crate::video_decoder::raw_extractor::AMediaExtractor_advance(ex);
                        }
                    }
                    Ok(DequeuedInputBufferResult::TryAgainLater) => {}
                    Err(e) => return Err(format!("音频 dequeue_input 失败: {e:?}")),
                }
            }

            // 取输出 PCM
            match codec.dequeue_output_buffer(timeout) {
                Ok(DequeuedOutputBufferInfoResult::Buffer(buf)) => {
                    const BUFFER_FLAG_END_OF_STREAM: u32 = 4;
                    let eos = buf.info().flags() & BUFFER_FLAG_END_OF_STREAM != 0;
                    let pts_us = buf.info().presentation_time_us();

                    // seek 落点校正：丢掉目标之前的声音。
                    if let Some(target) = seek_drop_until_us {
                        if pts_us >= 0 && pts_us < target {
                            codec.release_output_buffer(buf, false).ok();
                            continue;
                        }
                        seek_drop_until_us = None;
                        crate::mxprobe::log_public(&format!(
                            "[Audio][Seek] 落点到达 pts={pts_us}us（目标 {target}us）"
                        ));
                    }

                    if pts_us >= 0 && first_pts.is_none() {
                        first_pts = Some(pts_us);
                        // 音频首帧的 PTS 作为整条音轨的时间基准。
                        //
                        // 注意：seek 之后 first_pts 会被清空，于是这里会
                        // 用落点处第一块的真实 PTS 重新设基准 —— 这正是
                        // 我们要的，seek 后时钟从落点重新开始推进。
                        output.clock.set_base_pts(pts_us);
                        crate::mxprobe::log_public(&format!(
                            "[Audio] 基准 PTS = {pts_us}us"
                        ));
                    }

                    let pcm = buf.buffer();

                    // 写环形缓冲。写不下时小睡等待消费 —— 这是背压。
                    let mut off = 0usize;
                    while off < pcm.len() && !stop.load(Ordering::Relaxed) {
                        let n = output.write(&pcm[off..], pts_us.max(0));
                        off += n;
                        if n == 0 {
                            // 缓冲满：等 5ms 再试。期间若已停止就退出。
                            std::thread::sleep(Duration::from_millis(5));
                        }
                    }

                    codec.release_output_buffer(buf, false).ok();

                    if eos {
                        output_done = true;
                    }
                }
                Ok(DequeuedOutputBufferInfoResult::OutputFormatChanged) => {
                    let mut new_fmt = codec.output_format();
                    let new_sr = new_fmt.i32("sample-rate").unwrap_or(0);
                    let new_ch = new_fmt.i32("channel-count").unwrap_or(0);
                    crate::mxprobe::log_public(&format!(
                        "[Audio] 输出格式变化: sample-rate={new_sr} channels={new_ch}"
                    ));
                    if new_sr > 0 && new_ch > 0 {
                        output.clock.set_format(new_sr as u32, new_ch as u32, 2);
                    }
                }
                Ok(DequeuedOutputBufferInfoResult::OutputBuffersChanged) => {}
                Ok(DequeuedOutputBufferInfoResult::TryAgainLater) => {}
                Err(e) => return Err(format!("音频 dequeue_output 失败: {e:?}")),
            }
        }

        codec.stop().ok();
        crate::video_decoder::raw_extractor::AMediaExtractor_delete(ex);
        crate::mxprobe::log_public(&format!(
            "[Audio] 解码结束, underrun={}",
            output.underrun_count()
        ));
    }

    Ok(())
}

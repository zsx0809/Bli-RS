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
// ==============================================================================

use std::sync::atomic::{AtomicBool, Ordering};
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

        let handle = std::thread::spawn(move || {
            if let Err(e) = decode_loop(buffer, output, &stop_for_thread) {
                on_error(e);
            }
        });

        Self {
            stop,
            handle: Some(handle),
        }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn decode_loop(
    buffer: Arc<crate::stream_source::SharedBuffer>,
    output: Arc<AudioOutput>,
    stop: &AtomicBool,
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

        let mime = format
            .str("mime")
            .map(|s| s.to_string())
            .unwrap_or_else(|| "audio/mp4a-latm".into());

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

        while !output_done && !stop.load(Ordering::Relaxed) {
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

                    if pts_us >= 0 && first_pts.is_none() {
                        first_pts = Some(pts_us);
                        // 音频首帧的 PTS 作为整条音轨的时间基准。
                        // 视频侧会用这个基准去对齐（两轨的 PTS 原点可能不同）。
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
                    crate::mxprobe::log_public("[Audio] 输出格式变化");
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

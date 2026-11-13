// ==============================================================================
// 音频输出 + 主时钟 (AAudio)
//
// 为什么音频必须是主时钟：
//   视频帧晚一点/早一点人眼几乎看不出来（几帧内无感），但音频只要出现
//   几毫秒的间隔不连续，耳朵立刻能听出"咔哒"。所以行业惯例是：
//   **以音频播放位置为唯一时间基准，视频帧去追音频**。
//   反过来（视频主时钟）会让音频不断被重采样或丢帧，听起来发抖。
//
// 数据流：
//   MediaCodec(audio, PCM 输出) → 环形缓冲 → AAudio 回调 → 扬声器
//
// 为什么用环形缓冲而不是"边解边塞"：
//   AAudio 的回调在**实时线程**上跑，绝对不能在回调里等待锁、分配内存
//   或做任何可能阻塞的事（会导致 underrun，听感是爆音）。
//   所以解码线程写、回调线程读，两边靠一个无锁的字节环形缓冲衔接。
// ==============================================================================

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ndk::audio::{
    AudioCallbackResult, AudioDirection, AudioFormat, AudioPerformanceMode, AudioSharingMode,
    AudioStream, AudioStreamBuilder, AudioUsage,
};

/// 环形缓冲：解码线程写、AAudio 回调线程读。
///
/// 用 Mutex 而不是真正的无锁结构，原因是：
///   临界区只有一次 memcpy，且 AAudio 回调线程优先级远高于解码线程，
///   持锁时间极短（微秒级），实测不会造成 underrun。
///   真正无锁实现（如 SPSC ring）要处理大量边界情况，
///   在这里收益不足以抵消出错风险。
struct Ring {
    buf: Vec<u8>,
    /// 读位置（绝对字节偏移）
    read: u64,
    /// 写位置（绝对字节偏移）
    write: u64,
}

impl Ring {
    fn new(capacity: usize) -> Self {
        Self {
            buf: vec![0u8; capacity],
            read: 0,
            write: 0,
        }
    }

    fn len(&self) -> u64 {
        self.write - self.read
    }

    fn cap(&self) -> u64 {
        self.buf.len() as u64
    }

    /// 写入数据，返回写入字节数（空间不足时只写一部分）
    fn push(&mut self, data: &[u8]) -> usize {
        let space = (self.cap() - self.len()) as usize;
        let n = data.len().min(space);
        for i in 0..n {
            let pos = ((self.write + i as u64) % self.cap()) as usize;
            self.buf[pos] = data[i];
        }
        self.write += n as u64;
        n
    }

    /// 读取数据，返回读取字节数
    fn pop(&mut self, out: &mut [u8]) -> usize {
        let avail = self.len() as usize;
        let n = out.len().min(avail);
        for i in 0..n {
            let pos = ((self.read + i as u64) % self.cap()) as usize;
            out[i] = self.buf[pos];
        }
        self.read += n as u64;
        n
    }

    fn clear(&mut self) {
        self.read = self.write;
    }
}

/// 播放时钟：音频输出侧与视频解码侧共享。
pub struct PlayClock {
    /// 已写入环形缓冲的累计字节数（解码侧维护）
    written_bytes: AtomicU64,
    /// 已从环形缓冲取走、真正送去播放的累计字节数（回调侧维护）
    played_bytes: AtomicU64,
    /// 每帧字节数（通道数 × 每样本字节数）
    frame_bytes: AtomicU64,
    /// 采样率
    sample_rate: AtomicU64,
    /// 暂停
    paused: AtomicBool,
    /// 已播放到的 PTS（微秒），由解码侧按"写入了多少"换算写入
    last_pts_us: AtomicI64,
    /// 音频轨道起始 PTS（用于对齐视频）
    base_pts_us: AtomicI64,
}

impl PlayClock {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            written_bytes: AtomicU64::new(0),
            played_bytes: AtomicU64::new(0),
            frame_bytes: AtomicU64::new(4),
            sample_rate: AtomicU64::new(48000),
            paused: AtomicBool::new(false),
            last_pts_us: AtomicI64::new(-1),
            base_pts_us: AtomicI64::new(0),
        })
    }

    /// 当前音频播放位置（微秒）。
    ///
    /// 这是音画同步的**唯一权威时间**：视频解码线程每帧拿它比较，
    /// 决定是等待还是快进丢帧。
    pub fn position_us(&self) -> i64 {
        let played = self.played_bytes.load(Ordering::Relaxed);
        let fb = self.frame_bytes.load(Ordering::Relaxed).max(1);
        let sr = self.sample_rate.load(Ordering::Relaxed).max(1);
        let frames = played / fb;
        self.base_pts_us.load(Ordering::Relaxed)
            + (frames as f64 / sr as f64 * 1_000_000.0) as i64
    }

    pub fn set_format(&self, sample_rate: u32, channel_count: u32, bytes_per_sample: u64) {
        self.sample_rate
            .store(sample_rate as u64, Ordering::Relaxed);
        self.frame_bytes.store(
            (channel_count as u64) * bytes_per_sample,
            Ordering::Relaxed,
        );
    }

    pub fn set_base_pts(&self, pts_us: i64) {
        self.base_pts_us.store(pts_us, Ordering::Relaxed);
    }

    /// 解码侧：记录"这段数据的 PTS 是多少"，随写入推进
    pub fn note_written(&self, bytes: u64, pts_us: i64) {
        self.written_bytes.fetch_add(bytes, Ordering::Relaxed);
        self.last_pts_us.store(pts_us, Ordering::Relaxed);
    }

    /// 回调侧：报告本次取走了多少字节
    fn note_played(&self, bytes: u64) {
        self.played_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// 丢帧/seek 后重置
    pub fn reset_to(&self, base_pts_us: i64) {
        self.played_bytes.store(0, Ordering::Relaxed);
        self.written_bytes.store(0, Ordering::Relaxed);
        self.base_pts_us.store(base_pts_us, Ordering::Relaxed);
    }

    pub fn set_paused(&self, p: bool) {
        self.paused.store(p, Ordering::Relaxed);
    }

    /// 诊断用：已消费字节数的原始值
    pub fn played_bytes_debug(&self) -> u64 {
        self.played_bytes.load(Ordering::Relaxed)
    }
}

/// 音频输出器：持有 AAudio 流与环形缓冲。
pub struct AudioOutput {
    ring: Arc<Mutex<Ring>>,
    pub clock: Arc<PlayClock>,
    underruns: Arc<AtomicU64>,
}

// Safety:
//   所有字段都是 Arc + Mutex/原子，本身 Send + Sync。
//   AAudio 流句柄在 open() 里启动后即 drop，不进入这个结构体
//   （AudioStream 内部是 NonNull，带上它会污染整个类型的线程约束）。
unsafe impl Send for AudioOutput {}
unsafe impl Sync for AudioOutput {}

impl AudioOutput {
    /// 打开音频输出流。
    ///
    /// `sample_rate` / `channel_count` 来自解码器的输出格式，
    /// 必须与 MediaCodec 解出的 PCM 一致，否则会变速或噪音。
    pub fn open(sample_rate: u32, channel_count: u32) -> Result<Self, String> {
        // 环形缓冲容量：约 1 秒的音频。
        //   太小 → 解码偶尔抖动就 underrun（爆音）
        //   太大 → seek 后残留旧音频，且暂停响应变慢
        let bytes_per_frame = channel_count as usize * 2; // 16bit
        let capacity = sample_rate as usize * bytes_per_frame;

        let ring = Arc::new(Mutex::new(Ring::new(capacity)));
        let clock = PlayClock::new();
        clock.set_format(sample_rate, channel_count, 2);
        let underruns = Arc::new(AtomicU64::new(0));

        let ring_cb = ring.clone();
        let clock_cb = clock.clone();
        let under_cb = underruns.clone();

        let stream = AudioStreamBuilder::new()
            .map_err(|e| format!("创建 AAudio builder 失败: {e:?}"))?
            .direction(AudioDirection::Output)
            .format(AudioFormat::PCM_I16)
            .channel_count(channel_count as i32)
            .sample_rate(sample_rate as i32)
            .performance_mode(AudioPerformanceMode::LowLatency)
            .sharing_mode(AudioSharingMode::Shared)
            .usage(AudioUsage::Media)
            .data_callback(Box::new(
                move |_stream: &AudioStream, data: *mut std::ffi::c_void, frames: i32| {
                    if data.is_null() || frames <= 0 {
                        return AudioCallbackResult::Continue;
                    }

                    let bytes = (frames as usize) * channel_count as usize * 2;
                    let out = unsafe {
                        std::slice::from_raw_parts_mut(data as *mut u8, bytes)
                    };

                    // 暂停：填静音但不推进时钟，避免"暂停时进度还在走"
                    if clock_cb.paused.load(Ordering::Relaxed) {
                        out.fill(0);
                        return AudioCallbackResult::Continue;
                    }

                    let got = match ring_cb.try_lock() {
                        Ok(mut r) => r.pop(out),
                        // 拿不到锁（解码线程正在写）：本次填静音，
                        // 绝不能阻塞 —— 实时线程一阻塞就爆音。
                        Err(_) => 0,
                    };

                    if got < bytes {
                        // 缓冲不够：剩余填静音，并计一次 underrun
                        out[got..].fill(0);
                        under_cb.fetch_add(1, Ordering::Relaxed);
                    }
                    clock_cb.note_played(got as u64);

                    AudioCallbackResult::Continue
                },
            ))
            .open_stream()
            .map_err(|e| format!("打开音频流失败: {e:?}"))?;

        stream
            .request_start()
            .map_err(|e| format!("启动音频流失败: {e:?}"))?;

        crate::mxprobe::log_public(&format!(
            "[Audio] AAudio 已开启 {sample_rate}Hz {channel_count}ch, buffer={}ms",
            (capacity as f64 / (sample_rate as f64 * bytes_per_frame as f64) * 1000.0) as i32
        ));

        // ================================================================
        // 关键：绝不能 drop(stream) —— 那会调用 AAudioStream_close()，
        // 流立刻被关闭，于是完全没声音；音频时钟也永远停在 0，
        // 视频侧每帧都判定"超前"而 sleep，看起来就是速度变慢。
        //
        // 实测踩过：为了绕开 AudioStream 的 !Send 约束直接 drop，
        // 代价是整条音频链路静默失效，且症状表现为"视频变慢"，
        // 极难从表象联想到音频。
        //
        // 正确做法：std::mem::forget 泄漏掉这个句柄。
        //   · AAudio 流本身由框架内部持有，只要不 close 就一直工作
        //   · 泄漏一个 8 字节结构体，代价可忽略
        //   · 进程退出时由内核回收，不会造成实质泄漏
        //
        // 这样 AudioStream 的 !Send 也不会污染 AudioOutput：
        // 句柄不再存在于结构体里（被遗忘了），剩下的成员全是
        // Arc + Mutex/原子，天然 Send + Sync。
        std::mem::forget(stream);

        Ok(Self {
            ring,
            clock,
            underruns,
        })
    }

    /// 解码线程写入 PCM。返回实际写入字节数（空间不足时会少写）。
    pub fn write(&self, pcm: &[u8], pts_us: i64) -> usize {
        let n = match self.ring.lock() {
            Ok(mut r) => r.push(pcm),
            Err(e) => e.into_inner().push(pcm),
        };
        self.clock.note_written(n as u64, pts_us);
        n
    }

    /// 缓冲里还有多少字节没播
    pub fn buffered(&self) -> u64 {
        match self.ring.lock() {
            Ok(r) => r.len(),
            Err(e) => e.into_inner().len(),
        }
    }

    pub fn capacity_bytes(&self) -> u64 {
        match self.ring.lock() {
            Ok(r) => r.cap(),
            Err(e) => e.into_inner().cap(),
        }
    }

    pub fn clear(&self) {
        match self.ring.lock() {
            Ok(mut r) => r.clear(),
            Err(e) => e.into_inner().clear(),
        }
    }

    /// 诊断：已消费的字节数。用来确认 AAudio 回调真的在跑。
    pub fn played_bytes(&self) -> u64 {
        self.clock.played_bytes_debug()
    }

    pub fn underrun_count(&self) -> u64 {
        self.underruns.load(Ordering::Relaxed)
    }

    pub fn pause(&self) {
        self.clock.set_paused(true);
    }

    pub fn resume(&self) {
        self.clock.set_paused(false);
    }
}



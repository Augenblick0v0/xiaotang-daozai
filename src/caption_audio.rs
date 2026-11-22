//! 实时字幕的音频采集：WASAPI 共享模式 → 16 kHz / 单声道 / s16le。
//!
//! 系统声音走默认播放设备的环回（WASAPI loopback），麦克风走默认录音设备。
//! 采集线程自己初始化 COM（MTA），设备与音频客户端都不跨线程；线程退出前
//! `CoUninitialize`。外部只拿到一个 `Receiver<Vec<i16>>`，每包固定 100 ms。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// 输出采样率：16 kHz。
const SAMPLE_RATE: u32 = 16_000;
/// 每帧样本数：100 ms @ 16 kHz = 1600 个 i16 = 3200 字节。
const FRAME_SAMPLES: usize = 1_600;

/// 采集来源。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureSource {
    /// 默认输出设备的系统声音环回。
    SystemLoopback,
    /// 默认录音设备（麦克风）。
    Microphone,
}

/// 一路正在运行的采集。
pub struct Capture {
    /// 每包 1600 个 i16（100 ms @ 16 kHz）。
    pub rx: Receiver<Vec<i16>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    label: String,
}

impl Capture {
    /// 启动采集；返回的 rx 每次给 1600 个 i16（100 ms @ 16 kHz）。
    pub fn start(source: CaptureSource) -> Result<Capture, String> {
        // 调试注入：实机验证时用 WAV 文件代替声卡，全程不向扬声器播放声音。
        if let Some(path) = debug_wav_path() {
            return start_wav(&path);
        }
        #[cfg(windows)]
        {
            platform::start(source)
        }
        #[cfg(not(windows))]
        {
            let _ = source;
            Err("实时字幕音频采集只支持 Windows（WASAPI）".to_string())
        }
    }

    /// 采集到的设备名 / 引擎格式描述，给页面显示用。
    pub fn device_label(&self) -> String {
        self.label.clone()
    }

    /// 请求停止（幂等），后台线程在 ~200 ms 内退出。
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// 调试注入入口：`DAOZAI_CAPTION_WAV` 指向 16 kHz / 单声道 / 16-bit WAV。
pub fn debug_wav_path() -> Option<PathBuf> {
    std::env::var_os("DAOZAI_CAPTION_WAV")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

/// 解析 WAV，返回 (采样率, 单声道样本)。只接受未压缩 16-bit PCM。
fn parse_wav(bytes: &[u8]) -> Result<(u32, Vec<i16>), String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("不是 RIFF/WAVE 文件".into());
    }
    let mut rate = 0u32;
    let mut channels = 0u16;
    let mut bits = 0u16;
    let mut pcm: Option<Vec<i16>> = None;
    let mut offset = 12usize;
    while offset + 8 <= bytes.len() {
        let id = &bytes[offset..offset + 4];
        let size = u32::from_le_bytes([
            bytes[offset + 4],
            bytes[offset + 5],
            bytes[offset + 6],
            bytes[offset + 7],
        ]) as usize;
        let body = offset + 8;
        let end = body.saturating_add(size).min(bytes.len());
        match id {
            b"fmt " if end >= body + 16 => {
                let tag = u16::from_le_bytes([bytes[body], bytes[body + 1]]);
                channels = u16::from_le_bytes([bytes[body + 2], bytes[body + 3]]);
                rate = u32::from_le_bytes([
                    bytes[body + 4],
                    bytes[body + 5],
                    bytes[body + 6],
                    bytes[body + 7],
                ]);
                bits = u16::from_le_bytes([bytes[body + 14], bytes[body + 15]]);
                if tag != 1 {
                    return Err(format!("只支持未压缩 PCM（format={tag}）"));
                }
            }
            b"data" => {
                let raw = &bytes[body..end];
                pcm = Some(
                    raw.chunks_exact(2)
                        .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
                        .collect(),
                );
            }
            _ => {}
        }
        offset = body + size + (size & 1);
    }
    if channels != 1 || bits != 16 {
        return Err(format!("需要 16-bit 单声道，实际 {bits}-bit / {channels} 声道"));
    }
    let samples = pcm.ok_or_else(|| "WAV 里没有 data 块".to_string())?;
    Ok((rate, samples))
}

fn start_wav(path: &Path) -> Result<Capture, String> {
    let bytes = fs::read(path).map_err(|e| format!("读不到 {}: {e}", path.display()))?;
    let (rate, samples) = parse_wav(&bytes)?;
    if rate != SAMPLE_RATE {
        return Err(format!("需要 {SAMPLE_RATE} Hz，实际 {rate} Hz"));
    }
    let (tx, rx) = channel::<Vec<i16>>();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let label = format!("调试音频 {}", path.file_name().and_then(|n| n.to_str()).unwrap_or("wav"));
    let looped = std::env::var_os("DAOZAI_CAPTION_WAV_LOOP").is_some();
    let handle = thread::spawn(move || {
        let pace = Duration::from_millis(100);
        loop {
            for chunk in samples.chunks(FRAME_SAMPLES) {
                if thread_stop.load(Ordering::SeqCst) || tx.send(chunk.to_vec()).is_err() {
                    return;
                }
                thread::sleep(pace);
            }
            if !looped {
                break;
            }
            // 循环之间留半秒静音，模拟真实说话的停顿，也让模型有机会收句。
            for _ in 0..5 {
                if thread_stop.load(Ordering::SeqCst) {
                    return;
                }
                thread::sleep(pace);
            }
        }
        // 文件播完保持静默，等上层停止（不额外制造声音）。
        while !thread_stop.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(50));
        }
    });
    Ok(Capture { rx, stop, handle: Some(handle), label })
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

// ---------------------------------------------------------------------------
// 纯函数（可单测）
// ---------------------------------------------------------------------------

/// 交错多声道 → 单声道：按帧取各声道均值；单声道原样返回。
pub fn downmix_to_mono(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    let frames = interleaved.len() / channels;
    let mut out = Vec::with_capacity(frames);
    for frame in interleaved.chunks_exact(channels) {
        let sum: f32 = frame.iter().copied().sum();
        out.push(sum / channels as f32);
    }
    out
}

/// 有状态线性重采样器的无状态入口：把 input 当作独立的一段处理。
///
/// 采集循环里不要每包调用这个函数（会重置相位、产生周期性爆音），而应持有
/// [`LinearResampler`] 跨包复用。
pub fn resample_linear(input: &[f32], from_hz: u32, to_hz: u32) -> Vec<f32> {
    let mut resampler = LinearResampler::new(from_hz, to_hz);
    resampler.process(input)
}

/// f32 [-1, 1] → i16，超出范围饱和到 ±32767（不是 ±32768）。
pub fn f32_to_i16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|&sample| {
            if sample.is_nan() {
                0
            } else {
                (sample * 32767.0).round().clamp(-32767.0, 32767.0) as i16
            }
        })
        .collect()
}

/// 跨包连续的线性重采样器。
///
/// 相位 `pos` 以「当前输入包」的样本下标为单位，允许落在 `[-1, 0)`：此时用
/// 上一包最后一个样本与本包第一个样本插值。这样包边界不会重置相位，也不会
/// 因缺少左邻样本而出现台阶。
struct LinearResampler {
    step: f64,
    pos: f64,
    prev: Option<f32>,
}

impl LinearResampler {
    fn new(from_hz: u32, to_hz: u32) -> Self {
        let step = if from_hz == 0 || to_hz == 0 {
            1.0
        } else {
            from_hz as f64 / to_hz as f64
        };
        Self {
            step,
            pos: 0.0,
            prev: None,
        }
    }

    fn process(&mut self, input: &[f32]) -> Vec<f32> {
        let mut out = Vec::with_capacity((input.len() as f64 / self.step).ceil() as usize + 1);
        if input.is_empty() || self.step <= 0.0 {
            return out;
        }
        let len = input.len();
        // 相位在上一包结束时已经折算成「相对本包首样本」（通常为负，表示落在
        // 上一包末尾与本包首样本之间）。极端步长跨过整包时再折一次。
        while self.pos >= len as f64 {
            self.pos -= len as f64;
        }
        loop {
            let pos = self.pos;
            if pos < 0.0 {
                let Some(prev) = self.prev else { break };
                let frac = (pos + 1.0) as f32;
                out.push(prev + (input[0] - prev) * frac);
                self.pos += self.step;
                continue;
            }
            let index = pos.floor() as usize;
            if index >= len {
                break;
            }
            let frac = (pos - index as f64) as f32;
            if frac <= f32::EPSILON {
                out.push(input[index]);
                self.pos += self.step;
                continue;
            }
            if index + 1 >= len {
                // 需要下一包的第一个样本才能插值，保留相位等下一包。
                break;
            }
            let a = input[index];
            let b = input[index + 1];
            out.push(a + (b - a) * frac);
            self.pos += self.step;
        }
        // 把本包坐标系里的相位折算到下一包：下一包第 0 个样本 = 本包第 len 个。
        self.pos -= len as f64;
        self.prev = input.last().copied();
        out
    }
}

// ---------------------------------------------------------------------------
// Windows / WASAPI 实现
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod platform {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{self, Sender};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use windows::core::GUID;
    use windows::Win32::Media::Audio::{
        eCapture, eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDevice,
        IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT,
        AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
        COINIT_MULTITHREADED, STGM_READ,
    };
    use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;

    use super::{
        downmix_to_mono, f32_to_i16, Capture, CaptureSource, LinearResampler, FRAME_SAMPLES,
        SAMPLE_RATE,
    };

    /// `WAVE_FORMAT_IEEE_FLOAT`
    const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
    /// `WAVE_FORMAT_PCM`
    const WAVE_FORMAT_PCM: u16 = 0x0001;
    /// `WAVE_FORMAT_EXTENSIBLE`
    const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
    /// `KSDATAFORMAT_SUBTYPE_IEEE_FLOAT`
    const SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
    /// `KSDATAFORMAT_SUBTYPE_PCM`
    const SUBTYPE_PCM: GUID = GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);
    /// `PKEY_Device_FriendlyName`
    const PKEY_DEVICE_FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
        fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
        pid: 14,
    };

    /// 引擎格式（GetMixFormat 的结果，初始化后仍要用来解释每包字节）。
    #[derive(Clone, Copy)]
    struct EngineFormat {
        sample_rate: u32,
        channels: usize,
        block_align: usize,
        bits: u16,
        float: bool,
    }

    /// COM 线程守卫：线程入口初始化 MTA，退出时成对 CoUninitialize。
    struct ComGuard;

    impl ComGuard {
        fn init() -> Result<Self, String> {
            let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            if hr.0 < 0 {
                return Err(format!("COM 初始化失败 (HRESULT 0x{:08X})", hr.0 as u32));
            }
            Ok(Self)
        }
    }

    impl Drop for ComGuard {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }

    fn hresult(context: &str, error: windows::core::Error) -> String {
        format!(
            "{context}失败 (HRESULT 0x{:08X}): {error}",
            error.code().0 as u32
        )
    }

    fn rate_label(hz: u32) -> String {
        if hz % 1000 == 0 {
            format!("{} kHz", hz / 1000)
        } else {
            format!("{:.1} kHz", hz as f64 / 1000.0)
        }
    }

    fn friendly_name(device: &IMMDevice, fallback: &str) -> String {
        unsafe {
            let Ok(store) = device.OpenPropertyStore(STGM_READ) else {
                return fallback.to_string();
            };
            let Ok(value) = store.GetValue(&PKEY_DEVICE_FRIENDLY_NAME) else {
                return fallback.to_string();
            };
            let name = value.to_string();
            if name.trim().is_empty() {
                fallback.to_string()
            } else {
                name
            }
        }
    }

    fn detect_format(ptr: *const WAVEFORMATEX, wfx: &WAVEFORMATEX) -> Result<EngineFormat, String> {
        // WAVEFORMATEX 是 packed(1)，先逐字段按值拷贝，避免取到未对齐字段的引用。
        let tag = wfx.wFormatTag;
        let channels = wfx.nChannels as usize;
        let sample_rate = wfx.nSamplesPerSec;
        let block_align = wfx.nBlockAlign as usize;
        let bits = wfx.wBitsPerSample;
        let cb_size = wfx.cbSize;
        if channels == 0 || sample_rate == 0 || block_align == 0 {
            return Err(format!(
                "音频引擎返回了无效格式：{channels}ch / {sample_rate} Hz / blockAlign {block_align}"
            ));
        }
        // 规格里的混合格式判断：IEEE_FLOAT，或 32 bit 且没有扩展头。
        let mut float = tag == WAVE_FORMAT_IEEE_FLOAT || (bits == 32 && cb_size == 0);
        if tag == WAVE_FORMAT_EXTENSIBLE {
            if cb_size < 22 {
                return Err(format!(
                    "WAVEFORMATEXTENSIBLE 扩展头过短（cbSize={cb_size}）"
                ));
            }
            // 整个结构按 packed(1) 读出，避免对未对齐字段取引用。
            let ext = unsafe { std::ptr::read_unaligned(ptr.cast::<WAVEFORMATEXTENSIBLE>()) };
            let subformat = ext.SubFormat;
            if subformat == SUBTYPE_IEEE_FLOAT {
                float = true;
            } else if subformat == SUBTYPE_PCM {
                float = false;
            } else {
                return Err(format!("不支持的音频子格式：{subformat:?}"));
            }
        } else if tag != WAVE_FORMAT_IEEE_FLOAT && tag != WAVE_FORMAT_PCM {
            return Err(format!("不支持的音频格式 wFormatTag=0x{tag:04X}"));
        }
        if !float && bits != 16 && bits != 32 {
            return Err(format!("不支持的 PCM 位深：{bits} bit"));
        }
        let bytes_per_sample = if float || bits == 32 { 4 } else { 2 };
        if block_align < channels * bytes_per_sample {
            return Err(format!(
                "音频格式的 blockAlign（{block_align}）小于 {channels}ch × {bytes_per_sample}B"
            ));
        }
        Ok(EngineFormat {
            sample_rate,
            channels,
            block_align,
            bits,
            float,
        })
    }

    fn open(
        source: CaptureSource,
    ) -> Result<(IAudioClient, IAudioCaptureClient, EngineFormat, String), String> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .map_err(|e| hresult("创建音频设备枚举器", e))?;
            let (flow, role) = match source {
                CaptureSource::SystemLoopback => (eRender, eConsole),
                CaptureSource::Microphone => (eCapture, eConsole),
            };
            let device = enumerator
                .GetDefaultAudioEndpoint(flow, role)
                .map_err(|e| {
                    if source == CaptureSource::SystemLoopback {
                        hresult("获取默认播放设备（系统声音环回需要至少一个输出设备）", e)
                    } else {
                        hresult("获取默认麦克风设备", e)
                    }
                })?;
            let fallback = match source {
                CaptureSource::SystemLoopback => "默认播放设备",
                CaptureSource::Microphone => "默认麦克风",
            };
            let name = friendly_name(&device, fallback);

            let client: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| hresult("激活音频客户端", e))?;
            let format_ptr = client
                .GetMixFormat()
                .map_err(|e| hresult("读取音频引擎混合格式", e))?;
            if format_ptr.is_null() {
                return Err("音频引擎返回了空的混合格式".to_string());
            }
            let wfx = std::ptr::read_unaligned(format_ptr);
            let format = match detect_format(format_ptr, &wfx) {
                Ok(format) => format,
                Err(error) => {
                    CoTaskMemFree(Some(format_ptr as _));
                    return Err(error);
                }
            };
            let flags = if source == CaptureSource::SystemLoopback {
                AUDCLNT_STREAMFLAGS_LOOPBACK
            } else {
                0
            };
            // 1 秒引擎缓冲，轮询 10 ms 取包；共享模式 periodicity 必须为 0。
            let initialized = client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                flags,
                10_000_000,
                0,
                format_ptr,
                None,
            );
            CoTaskMemFree(Some(format_ptr as _));
            initialized.map_err(|e| hresult("初始化音频客户端", e))?;

            let capture: IAudioCaptureClient = client
                .GetService()
                .map_err(|e| hresult("获取音频采集接口", e))?;
            let label = format!(
                "{name} · {} {}ch",
                rate_label(format.sample_rate),
                format.channels
            );
            Ok((client, capture, format, label))
        }
    }

    /// 一包引擎数据 → 单声道 f32。按 blockAlign 逐帧取，兼容尾部填充。
    unsafe fn packet_to_mono(data: *const u8, frames: u32, format: &EngineFormat) -> Vec<f32> {
        let bytes_per_sample = if format.float || format.bits == 32 {
            4
        } else {
            2
        };
        let mut interleaved = Vec::with_capacity(frames as usize * format.channels);
        for frame in 0..frames as usize {
            let base = frame * format.block_align;
            for channel in 0..format.channels {
                let offset = base + channel * bytes_per_sample;
                let sample = if format.float {
                    f32::from_le_bytes([
                        *data.add(offset),
                        *data.add(offset + 1),
                        *data.add(offset + 2),
                        *data.add(offset + 3),
                    ])
                } else if format.bits == 16 {
                    i16::from_le_bytes([*data.add(offset), *data.add(offset + 1)]) as f32 / 32768.0
                } else {
                    i32::from_le_bytes([
                        *data.add(offset),
                        *data.add(offset + 1),
                        *data.add(offset + 2),
                        *data.add(offset + 3),
                    ]) as f32
                        / 2_147_483_648.0
                };
                interleaved.push(sample);
            }
        }
        downmix_to_mono(&interleaved, format.channels)
    }

    fn run_capture(
        source: CaptureSource,
        stop: &AtomicBool,
        tx: &Sender<Vec<i16>>,
        ready: Sender<Result<String, String>>,
    ) -> Result<(), String> {
        let (client, capture, format, label) = match open(source) {
            Ok(opened) => opened,
            Err(error) => {
                let _ = ready.send(Err(error.clone()));
                return Err(error);
            }
        };
        if let Err(error) = unsafe { client.Start() } {
            let text = hresult("启动音频采集", error);
            let _ = ready.send(Err(text.clone()));
            return Err(text);
        }
        // 设备与客户端都就绪后才把 label 交给 start()，避免把半初始化状态当成功。
        let _ = ready.send(Ok(label));

        let mut resampler = LinearResampler::new(format.sample_rate, SAMPLE_RATE);
        let mut pending: Vec<i16> = Vec::with_capacity(FRAME_SAMPLES * 2);
        'capture: loop {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let packet = match unsafe { capture.GetNextPacketSize() } {
                Ok(size) => size,
                Err(_) => break, // 设备被拔掉/失效：结束线程，rx 断开。
            };
            if packet == 0 {
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            let mut size = packet;
            while size > 0 {
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                if unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None) }
                    .is_err()
                {
                    break 'capture;
                }
                if frames > 0 {
                    let silent =
                        data.is_null() || (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
                    let mono = if silent {
                        vec![0.0f32; frames as usize]
                    } else {
                        unsafe { packet_to_mono(data, frames, &format) }
                    };
                    let resampled = resampler.process(&mono);
                    pending.extend_from_slice(&f32_to_i16(&resampled));
                    while pending.len() >= FRAME_SAMPLES {
                        let frame: Vec<i16> = pending.drain(..FRAME_SAMPLES).collect();
                        if tx.send(frame).is_err() {
                            // 接收端已 drop：立刻退出，不再采集。
                            let _ = unsafe { capture.ReleaseBuffer(frames) };
                            break 'capture;
                        }
                    }
                }
                let _ = unsafe { capture.ReleaseBuffer(frames) };
                if stop.load(Ordering::SeqCst) {
                    break 'capture;
                }
                size = match unsafe { capture.GetNextPacketSize() } {
                    Ok(next) => next,
                    Err(_) => 0,
                };
            }
        }
        let _ = unsafe { client.Stop() };
        Ok(())
    }

    pub(super) fn start(source: CaptureSource) -> Result<Capture, String> {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<String, String>>();
        let (tx, rx) = mpsc::channel::<Vec<i16>>();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = thread::Builder::new()
            .name("caption-audio".to_string())
            .spawn(move || {
                let _com = match ComGuard::init() {
                    Ok(guard) => guard,
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                let _ = run_capture(source, &thread_stop, &tx, ready_tx);
            })
            .map_err(|e| format!("创建音频采集线程失败: {e}"))?;

        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(label)) => Ok(Capture {
                rx,
                stop,
                handle: Some(handle),
                label,
            }),
            Ok(Err(error)) => {
                stop.store(true, Ordering::SeqCst);
                let _ = handle.join();
                Err(error)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                stop.store(true, Ordering::SeqCst);
                // 设备枚举可能卡在驱动里，这里不 join，避免 start() 永久阻塞。
                Err("等待音频设备初始化超时（5 秒）".to_string())
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                stop.store(true, Ordering::SeqCst);
                let _ = handle.join();
                Err("音频采集线程在初始化前退出".to_string())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;
    use std::time::{Duration, Instant};

    fn sine(freq: f32, rate: u32, samples: usize) -> Vec<f32> {
        (0..samples)
            .map(|i| (2.0 * PI * freq * i as f32 / rate as f32).sin())
            .collect()
    }

    fn rms(samples: &[f32]) -> f32 {
        (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
    }

    #[test]
    fn downmix_to_mono_averages_stereo_and_passes_mono_through() {
        let stereo = [0.0, 1.0, 0.5, -0.5, 1.0, 1.0];
        assert_eq!(downmix_to_mono(&stereo, 2), vec![0.5, 0.0, 1.0]);
        let mono = [0.25, -0.75];
        assert_eq!(downmix_to_mono(&mono, 1), mono.to_vec());
    }

    #[test]
    fn resample_linear_decimates_by_three_and_preserves_dc() {
        let dc = vec![0.25f32; 4_800];
        let out = resample_linear(&dc, 48_000, 16_000);
        assert!(
            (1_599..=1_601).contains(&out.len()),
            "48k→16k 长度应约为 1/3，实际 {}",
            out.len()
        );
        assert!(
            out.iter().all(|s| (*s - 0.25).abs() < 1e-6),
            "直流分量必须保持"
        );
    }

    #[test]
    fn resample_linear_keeps_1khz_sine_amplitude_within_5_percent() {
        for from_hz in [48_000u32, 44_100] {
            let input = sine(1_000.0, from_hz, from_hz as usize / 10);
            let output = resample_linear(&input, from_hz, 16_000);
            let expected = input.len() as f64 * 16_000.0 / from_hz as f64;
            assert!(
                (output.len() as f64 - expected).abs() <= 2.0,
                "{from_hz}→16k 长度 {} 与预期 {expected:.1} 偏差过大",
                output.len()
            );
            let input_rms = rms(&input);
            let output_rms = rms(&output);
            let error = (output_rms - input_rms).abs() / input_rms;
            assert!(
                error < 0.05,
                "{from_hz}→16k 1 kHz 正弦幅度误差 {:.2}% 超过 5%（输入 RMS {input_rms:.5}，输出 RMS {output_rms:.5}）",
                error * 100.0
            );
        }
    }

    #[test]
    fn resampler_phase_is_continuous_across_packet_boundaries() {
        let input = sine(997.0, 44_100, 4_410);
        let single = resample_linear(&input, 44_100, 16_000);
        let mut resampler = LinearResampler::new(44_100, 16_000);
        let mut chunked = Vec::new();
        for chunk in [&input[..1_000], &input[1_000..1_777], &input[1_777..]] {
            chunked.extend(resampler.process(chunk));
        }
        assert_eq!(single.len(), chunked.len(), "分包的输出长度应与整段一致");
        for (index, (a, b)) in single.iter().zip(&chunked).enumerate() {
            assert!(
                (a - b).abs() < 1e-5,
                "第 {index} 个样本在分包处出现跳变：{a} vs {b}"
            );
        }
    }

    #[test]
    fn f32_to_i16_scales_and_saturates_at_32767() {
        assert_eq!(
            f32_to_i16(&[1.0, -1.0, 0.0, 1.5, -1.5]),
            vec![32767, -32767, 0, 32767, -32767]
        );
        assert_eq!(f32_to_i16(&[f32::NAN]), vec![0]);
    }

    /// 真机验证：需要默认输出设备，且本机正在播放声音（否则环回全是静音）。
    #[cfg(windows)]
    #[test]
    #[ignore = "需要本机正在播放声音；验证 WASAPI 默认输出设备环回采集"]
    fn live_loopback_captures_audio() {
        let capture =
            Capture::start(CaptureSource::SystemLoopback).expect("启动系统声音环回采集失败");
        println!("device_label = {}", capture.device_label());

        let mut total = 0usize;
        let mut nonzero = 0usize;
        let deadline = Instant::now() + Duration::from_secs(5);
        while total < 32_000 && Instant::now() < deadline {
            match capture.rx.recv_timeout(Duration::from_millis(500)) {
                Ok(frame) => {
                    assert_eq!(frame.len(), 1_600, "每帧必须是 100 ms / 1600 个 i16");
                    total += frame.len();
                    nonzero += frame.iter().filter(|sample| **sample != 0).count();
                }
                Err(_) => break,
            }
        }
        capture.stop();
        assert_eq!(
            total, 32_000,
            "2 秒应收到 20 帧 / 32000 个样本，实际收到 {total}"
        );
        let ratio = nonzero as f64 / total as f64;
        println!(
            "收到 {total} 个样本，非零样本 {nonzero}（{:.2}%）",
            ratio * 100.0
        );
        assert!(
            ratio >= 0.05,
            "非零样本仅 {:.2}%，本机可能没有声音在播放",
            ratio * 100.0
        );
    }
}

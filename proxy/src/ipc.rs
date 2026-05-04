// IPC server: implements PotPlayer's `-IPC Whisper<id>` protocol.
//
// PotPlayer creates `\\.\pipe\Whisper<id>` (server side, byte-mode duplex) and
// spawns `main64.exe -IPC Whisper<id>`. We connect as client and:
//
//  1. Read LOAD_MODEL frame (code 0x10, 524-byte UTF-16 path).
//  2. Reply with a synthesized whisper.cpp-style init log + READY frame.
//  3. Loop receiving CONVERT (0x20) header + audio (0x21) frames.
//  4. Forward each audio chunk to the configured Whisper API as WAV.
//  5. Reply with SEGMENT_TIMING (0x20000) + SEGMENT_TEXT (0x20001) per segment,
//     then CONVERT_END (0x22) per request.
//
// Concurrency:
//   - Main thread owns the pipe handle and does ALL pipe I/O (PeekNamedPipe
//     based read + WriteFile). Necessary because Windows pipes don't tolerate
//     concurrent ReadFile/WriteFile on the same handle from different threads.
//   - One worker thread (configurable via WHISPER_PROXY_WORKERS) does the slow
//     HTTP call; results come back to main via mpsc and are written.

use std::env;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::iter::once;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_GENERIC_READ, FILE_GENERIC_WRITE, OPEN_EXISTING,
};

use crate::config::Config;
use crate::whisper_client::{self, TranscribeRequest};

type BOOL = i32;

// ---------- helpers ----------

fn null_handle() -> HANDLE {
    std::ptr::null_mut()
}

fn to_wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(once(0)).collect()
}

fn open_pipe_client(name: &str) -> std::io::Result<File> {
    let wide = to_wide(name);
    // PotPlayer creates the pipe synchronously before spawning us, but tolerate
    // brief races on slow machines.
    for _ in 0..50 {
        let h = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_GENERIC_READ | FILE_GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                null_handle(),
            )
        };
        if h != INVALID_HANDLE_VALUE && h != null_handle() {
            return Ok(unsafe { File::from_raw_handle(h as RawHandle) });
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Err(std::io::Error::last_os_error())
}

// ---------- logging ----------

fn log_dir() -> PathBuf {
    env::var_os("TEMP")
        .or_else(|| env::var_os("TMP"))
        .or_else(|| env::var_os("LOCALAPPDATA"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn ipc_log_path() -> PathBuf {
    log_dir().join("whisper-proxy-ipc.log")
}

struct Logger {
    file: Mutex<File>,
    started: Instant,
}

impl Logger {
    fn new(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Logger { file: Mutex::new(file), started: Instant::now() })
    }
    fn line(&self, msg: &str) {
        let ms = self.started.elapsed().as_millis();
        let mut g = self.file.lock().unwrap();
        let _ = writeln!(g, "[{ms:>8}ms] {msg}");
        let _ = g.flush();
    }
}

// ---------- WAV / API ----------

fn make_wav_f32_mono_16k(samples_bytes: &[u8]) -> Vec<u8> {
    const SR: u32 = 16000;
    const CH: u16 = 1;
    const BITS: u16 = 32;
    let block_align: u16 = CH * (BITS / 8);
    let byte_rate: u32 = SR * (block_align as u32);
    let data_len = samples_bytes.len() as u32;
    let riff_len = 36 + data_len;
    let mut w = Vec::with_capacity(44 + samples_bytes.len());
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&riff_len.to_le_bytes());
    w.extend_from_slice(b"WAVE");
    w.extend_from_slice(b"fmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
    w.extend_from_slice(&CH.to_le_bytes());
    w.extend_from_slice(&SR.to_le_bytes());
    w.extend_from_slice(&byte_rate.to_le_bytes());
    w.extend_from_slice(&block_align.to_le_bytes());
    w.extend_from_slice(&BITS.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());
    w.extend_from_slice(samples_bytes);
    w
}

// 실시간 chunk → API 호출. 공통 whisper_client::transcribe wrapping.
// 빈 텍스트 segment는 자막 노이즈가 되므로 여기서 한 번 더 거른다.
fn transcribe_chunk(cfg: &Config, wav: &[u8], language: &str) -> Result<Vec<whisper_client::Segment>, String> {
    let req = TranscribeRequest {
        file_name: "chunk.wav",
        file_bytes: wav,
        mime: "audio/wav",
        language: if language.is_empty() { None } else { Some(language) },
        prompt: None,
        translate: false,
    };
    let result = whisper_client::transcribe(cfg, &req)?;
    let mut segs: Vec<whisper_client::Segment> = result
        .segments
        .into_iter()
        .map(|mut s| {
            s.text = s.text.trim().to_string();
            s
        })
        .filter(|s| !s.text.is_empty())
        .collect();
    if segs.is_empty() {
        let t = result.full_text.trim().to_string();
        if !t.is_empty() {
            segs.push(whisper_client::Segment { start: 0.0, end: 0.0, text: t });
        }
    }
    Ok(segs)
}

// ---------- init handshake ----------
//
// PotPlayer's stock main64.exe wraps whisper.cpp and forwards the library's
// init log lines to the player UI as frames (code 0x10000), then signals
// completion with a READY frame (code 0x11). We don't actually load a model
// (transcription is remote), so we synthesize the equivalent frame stream.
//
// The text content matches whisper.cpp's standard init output (whisper.cpp is
// MIT-licensed: https://github.com/ggerganov/whisper.cpp). The exact field
// values are cosmetic — PotPlayer only gates on the trailing READY frame
// before it starts sending audio.

const FRAME_MAGIC: u32 = 0x11111111;

// PotPlayer → 우리 (request)
const FRAME_LOAD_MODEL: u32 = 0x10;
const FRAME_CONVERT: u32 = 0x20;
const FRAME_AUDIO: u32 = 0x21;

// 우리 → PotPlayer (response)
const FRAME_LOG: u32 = 0x10000;
const FRAME_READY: u32 = 0x11;
const FRAME_CONVERT_END: u32 = 0x22;
const FRAME_SEGMENT_TIMING: u32 = 0x20000;
const FRAME_SEGMENT_TEXT: u32 = 0x20001;

// 영상 SEEK 감지: 인접 chunk의 ts_ms 차이가 이 임계값 이상이면 새 위치로 간주.
// 너무 작으면 정상 chunk 진행도 SEEK으로 오인, 너무 크면 짧은 jump 놓침.
const SEEK_THRESHOLD_MS: i64 = 10_000;

const INIT_LOG_LINES: &[&str] = &[
    "whisper_init_from_file_with_params_no_state: loading model from '<remote API>'",
    "whisper_init_with_params_no_state: use gpu    = 1",
    "whisper_init_with_params_no_state: flash attn = 1",
    "whisper_init_with_params_no_state: gpu_device = 0",
    "whisper_init_with_params_no_state: dtw        = 0",
    "whisper_init_with_params_no_state: devices    = 2",
    "whisper_init_with_params_no_state: backends   = 2",
    "whisper_model_load: loading model",
    "whisper_model_load: n_vocab       = 51865",
    "whisper_model_load: n_audio_ctx   = 1500",
    "whisper_model_load: n_audio_state = 384",
    "whisper_model_load: n_audio_head  = 6",
    "whisper_model_load: n_audio_layer = 4",
    "whisper_model_load: n_text_ctx    = 448",
    "whisper_model_load: n_text_state  = 384",
    "whisper_model_load: n_text_head   = 6",
    "whisper_model_load: n_text_layer  = 4",
    "whisper_model_load: n_mels        = 80",
    "whisper_model_load: ftype         = 1",
    "whisper_model_load: qntvr         = 0",
    "whisper_model_load: type          = 1 (tiny)",
    "whisper_model_load: adding 1608 extra tokens",
    "whisper_model_load: n_langs       = 99",
    "whisper_model_load:      Vulkan0 total size =    77.11 MB",
    "whisper_model_load: model size    =   77.11 MB",
    "whisper_backend_init_gpu: device 0: Vulkan0 (type: 1)",
    "whisper_backend_init_gpu: found GPU device 0: Vulkan0 (type: 1, cnt: 0)",
    "whisper_backend_init_gpu: using Vulkan0 backend",
    "whisper_init_state: kv self size  =    3.15 MB",
    "whisper_init_state: kv cross size =    9.44 MB",
    "whisper_init_state: kv pad  size  =    2.36 MB",
    "whisper_init_state: compute buffer (conv)   =   14.17 MB",
    "whisper_init_state: compute buffer (encode) =   17.72 MB",
    "whisper_init_state: compute buffer (cross)  =   13.11 MB",
    "whisper_init_state: compute buffer (decode) =   96.83 MB",
];

fn push_frame(out: &mut Vec<u8>, code: u32, body: &[u8]) {
    out.extend_from_slice(&FRAME_MAGIC.to_le_bytes());
    out.extend_from_slice(&code.to_le_bytes());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
}

fn build_init_response() -> Vec<u8> {
    // 합성 출력 크기 추정: 12B 헤더 * 36 frames + 평균 60B 본문
    let mut out = Vec::with_capacity(12 * 36 + 60 * INIT_LOG_LINES.len());
    let mut body = Vec::with_capacity(128);
    for line in INIT_LOG_LINES {
        // 각 로그 frame body: "<text>\n\0" (whisper.cpp의 newline + null terminator)
        body.clear();
        body.extend_from_slice(line.as_bytes());
        body.push(b'\n');
        body.push(0);
        push_frame(&mut out, FRAME_LOG, &body);
    }
    push_frame(&mut out, FRAME_READY, &[0]);
    out
}

// ---------- entry ----------

pub fn run(ipc_name: &str) -> i32 {
    let dir = env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));

    let log = match Logger::new(&ipc_log_path()) {
        Ok(l) => Arc::new(l),
        Err(e) => { eprintln!("ipc: log open: {e}"); return 40; }
    };
    log.line(&format!("=== serve start, pipe={ipc_name} pid={}", std::process::id()));

    let cfg = crate::config::load(&dir);
    log.line(&format!("config: url={} model={} timeout={}", cfg.url, cfg.model, cfg.timeout));

    let pot_pipe = format!("\\\\.\\pipe\\{ipc_name}");
    let pot = match open_pipe_client(&pot_pipe) {
        Ok(f) => f,
        Err(e) => { log.line(&format!("connect {pot_pipe}: {e}")); return 41; }
    };
    log.line(&format!("connected {pot_pipe}"));

    use std::ptr::null_mut;
    #[link(name = "kernel32")]
    extern "system" {
        fn ReadFile(h: HANDLE, buf: *mut u8, n: u32, read: *mut u32, ov: *mut std::ffi::c_void) -> BOOL;
        fn WriteFile(h: HANDLE, buf: *const u8, n: u32, written: *mut u32, ov: *mut std::ffi::c_void) -> BOOL;
        fn PeekNamedPipe(h: HANDLE, buf: *mut u8, n: u32, read: *mut u32,
                         total_avail: *mut u32, left_in_msg: *mut u32) -> BOOL;
        fn GetLastError() -> u32;
    }
    let h = pot.as_raw_handle() as HANDLE;

    let bytes_avail = || -> u32 {
        let mut avail: u32 = 0;
        let ok = unsafe { PeekNamedPipe(h, null_mut(), 0, null_mut(), &mut avail as *mut u32, null_mut()) };
        if ok == 0 { 0 } else { avail }
    };
    let read_exact = |out: &mut [u8]| -> std::io::Result<()> {
        let mut got = 0;
        while got < out.len() {
            let mut n: u32 = 0;
            let ok = unsafe {
                ReadFile(h, out[got..].as_mut_ptr(), (out.len() - got) as u32, &mut n as *mut u32, null_mut())
            };
            if ok == 0 {
                return Err(std::io::Error::from_raw_os_error(unsafe { GetLastError() } as i32));
            }
            if n == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "eof"));
            }
            got += n as usize;
        }
        Ok(())
    };
    let write_all = |buf: &[u8]| -> std::io::Result<()> {
        let mut sent = 0;
        while sent < buf.len() {
            let mut n: u32 = 0;
            let ok = unsafe {
                WriteFile(h, buf[sent..].as_ptr(), (buf.len() - sent) as u32, &mut n as *mut u32, null_mut())
            };
            if ok == 0 {
                return Err(std::io::Error::from_raw_os_error(unsafe { GetLastError() } as i32));
            }
            sent += n as usize;
        }
        Ok(())
    };
    let read_frame = || -> std::io::Result<(u32, Vec<u8>)> {
        let mut hdr = [0u8; 12];
        read_exact(&mut hdr)?;
        let magic = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
        let code = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
        let blen = u32::from_le_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]) as usize;
        if magic != FRAME_MAGIC {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData,
                format!("bad magic 0x{magic:x}")));
        }
        let mut body = vec![0u8; blen];
        if blen > 0 { read_exact(&mut body)?; }
        Ok((code, body))
    };

    // --- LOAD_MODEL ---
    let (code, body) = match read_frame() {
        Ok(v) => v,
        Err(e) => { log.line(&format!("read LOAD_MODEL: {e}")); return 42; }
    };
    if code != FRAME_LOAD_MODEL {
        log.line(&format!("expected LOAD_MODEL (0x{FRAME_LOAD_MODEL:x}), got 0x{code:x}"));
        return 43;
    }
    log.line(&format!("LOAD_MODEL body len={}", body.len()));

    let init_response = build_init_response();
    if let Err(e) = write_all(&init_response) {
        log.line(&format!("send init response: {e}"));
        return 44;
    }
    log.line(&format!("init response sent ({} bytes)", init_response.len()));

    // --- worker pool ---
    use std::sync::mpsc;
    let cfg_arc = Arc::new(cfg);
    let log_arc = log.clone();

    let latest_ts: Arc<std::sync::atomic::AtomicU32> = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let seek_epoch: Arc<std::sync::atomic::AtomicU32> = Arc::new(std::sync::atomic::AtomicU32::new(0));

    struct Job { ts_ms: u32, flag: u32, status: u32, language: String, audio: Vec<u8> }
    let (job_tx, job_rx) = mpsc::channel::<Job>();
    let job_rx = Arc::new(Mutex::new(job_rx));

    type RespFrames = Vec<(u32, Vec<u8>)>;
    let (resp_tx, resp_rx) = mpsc::channel::<RespFrames>();

    // Single worker is best — PotPlayer requires response order to match request
    // order, and stale chunks are skipped via drift-from-latest_ts check.
    let n_workers = std::env::var("WHISPER_PROXY_WORKERS")
        .ok().and_then(|s| s.parse::<usize>().ok()).unwrap_or(1).max(1);
    log_arc.line(&format!("starting {n_workers} workers"));

    for wi in 0..n_workers {
        let job_rx = job_rx.clone();
        let cfg_arc = cfg_arc.clone();
        let log_arc = log_arc.clone();
        let seek_epoch = seek_epoch.clone();
        let resp_tx = resp_tx.clone();
        std::thread::spawn(move || {
            loop {
                let job = {
                    let g = job_rx.lock().unwrap();
                    match g.recv() {
                        Ok(j) => j,
                        Err(_) => break,
                    }
                };
                let started = std::time::Instant::now();
                // STALE 판정은 SEEK epoch 기준.
                // 시간 drift 기준(과거 5초)은 SEEK 직후 정상 chunk까지 drop했음:
                // PotPlayer가 SEEK 위치(t)와 다음 chunk(t+5~10s)를 빠르게 보내면
                // worker가 t를 transcribe하는 동안 latest_ts가 t+N으로 갱신되어
                // |t+N - t| > 5000 이 되어 SEEK 위치 자막이 drop됐음.
                // epoch 기준으로 바꾸면 같은 SEEK 시점의 chunk는 모두 처리되고,
                // 그 사이 또 SEEK이 발생한 경우(epoch 증가)에만 stale.
                let cur_epoch = seek_epoch.load(std::sync::atomic::Ordering::Relaxed);
                let stale_pre = job.status < cur_epoch;

                let segments = if stale_pre {
                    log_arc.line(&format!("  [w{wi}] STALE-PRE skip ts={} (job.epoch={} cur={})",
                        job.ts_ms, job.status, cur_epoch));
                    Vec::new()
                } else {
                    let wav = make_wav_f32_mono_16k(&job.audio);
                    let segs = match transcribe_chunk(&cfg_arc, &wav, &job.language) {
                        Ok(s) => s,
                        Err(e) => {
                            log_arc.line(&format!("  [w{wi}] API err: {e}"));
                            Vec::new()
                        }
                    };
                    let cur_epoch2 = seek_epoch.load(std::sync::atomic::Ordering::Relaxed);
                    if job.status < cur_epoch2 {
                        log_arc.line(&format!("  [w{wi}] STALE-POST drop ts={} (job.epoch={} cur={})",
                            job.ts_ms, job.status, cur_epoch2));
                        Vec::new()
                    } else {
                        segs
                    }
                };
                let elapsed_ms = started.elapsed().as_millis();
                log_arc.line(&format!("  [w{wi}] ts={} flag={} segs={} took={}ms",
                    job.ts_ms, job.flag, segments.len(), elapsed_ms));
                for (i, seg) in segments.iter().enumerate() {
                    log_arc.line(&format!(
                        "  [w{wi}]   seg{i} {:.2}-{:.2}s text={:?}",
                        seg.start, seg.end, seg.text
                    ));
                }

                let mut frames: RespFrames = Vec::with_capacity(segments.len() * 2 + 1);
                for seg in &segments {
                    let start_ms = (seg.start * 1000.0) as u64 + job.ts_ms as u64;
                    let end_ms = (seg.end * 1000.0) as u64 + job.ts_ms as u64;
                    let mut t = vec![0u8; 24];
                    // SEGMENT_TIMING body layout (24B):
                    //   [0..4)  status — seek epoch, must match PotPlayer's current epoch
                    //   [4..8)  flag   — echoes input header A
                    //   [8..16) start_ms (u64 LE)
                    //   [16..24) end_ms (u64 LE)
                    t[0..4].copy_from_slice(&job.status.to_le_bytes());
                    t[4..8].copy_from_slice(&job.flag.to_le_bytes());
                    t[8..16].copy_from_slice(&start_ms.to_le_bytes());
                    t[16..24].copy_from_slice(&end_ms.to_le_bytes());
                    frames.push((FRAME_SEGMENT_TIMING, t));
                    let mut text = seg.text.clone().into_bytes();
                    text.push(0);
                    frames.push((FRAME_SEGMENT_TEXT, text));
                }
                let lang_pair = format!("{}/{}\0", job.language, job.language);
                frames.push((FRAME_CONVERT_END, lang_pair.into_bytes()));
                if resp_tx.send(frames).is_err() { break; }
            }
        });
    }
    drop(resp_tx);
    let tx = job_tx;

    // --- main loop: multiplex pipe read + response write ---
    loop {
        let mut did_work = false;

        // 1) drain any completed responses
        loop {
            match resp_rx.try_recv() {
                Ok(frames) => {
                    did_work = true;
                    let mut frame_count = 0;
                    for (code, body) in &frames {
                        let mut hdr = [0u8; 12];
                        hdr[0..4].copy_from_slice(&FRAME_MAGIC.to_le_bytes());
                        hdr[4..8].copy_from_slice(&code.to_le_bytes());
                        hdr[8..12].copy_from_slice(&(body.len() as u32).to_le_bytes());
                        if write_all(&hdr).is_err() { break; }
                        if !body.is_empty() && write_all(body).is_err() { break; }
                        frame_count += 1;
                    }
                    log_arc.line(&format!("  [main] wrote {frame_count} frames"));
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }

        // 2) handle one incoming CONVERT (header + audio) if available
        if bytes_avail() >= 12 {
            did_work = true;
            let (code, body) = match read_frame() {
                Ok(v) => v,
                Err(e) => { log_arc.line(&format!("read frame end: {e}")); break; }
            };
            if code == FRAME_CONVERT {
                if body.len() < 44 { continue; }
                let lang_end = body[..32].iter().position(|&b| b == 0).unwrap_or(32);
                let language = String::from_utf8_lossy(&body[..lang_end]).to_string();
                // CONVERT header (44B) layout:
                //   [32..36)  A — flag (0=continuation, 2=preliminary)
                //   [36..40)  B — ts_ms (normal case)
                //   [40..44)  C — usually 0; if non-zero, takes precedence as ts_ms
                let a = u32::from_le_bytes([body[32], body[33], body[34], body[35]]);
                let b = u32::from_le_bytes([body[36], body[37], body[38], body[39]]);
                let c = u32::from_le_bytes([body[40], body[41], body[42], body[43]]);
                let flag = a;
                let ts_ms = if c != 0 { c } else { b };

                // Seek detection: if ts jumped >10s relative to previous, increment
                // epoch. PotPlayer matches response.status to its current epoch.
                let prev_ts = latest_ts.load(std::sync::atomic::Ordering::Relaxed);
                if prev_ts != 0 {
                    let delta = (ts_ms as i64 - prev_ts as i64).abs();
                    if delta > SEEK_THRESHOLD_MS {
                        let old = seek_epoch.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        log_arc.line(&format!("  SEEK detected: prev_ts={prev_ts} new_ts={ts_ms} epoch {} -> {}",
                            old, old + 1));
                    }
                }
                let status = seek_epoch.load(std::sync::atomic::Ordering::Relaxed);

                let (code2, audio) = match read_frame() {
                    Ok(v) => v,
                    Err(e) => { log_arc.line(&format!("read audio: {e}")); break; }
                };
                if code2 != FRAME_AUDIO {
                    log_arc.line(&format!("expected audio (0x{FRAME_AUDIO:x}), got 0x{code2:x}"));
                    continue;
                }
                log_arc.line(&format!(
                    "recv CONVERT lang={language} A={a} B={b} C={c} -> status={status} flag={flag} ts_ms={ts_ms} audio={}B",
                    audio.len()
                ));
                latest_ts.store(ts_ms, std::sync::atomic::Ordering::Relaxed);
                if tx.send(Job { ts_ms, flag, status, language, audio }).is_err() {
                    log_arc.line("worker channel closed");
                    break;
                }
            } else {
                log_arc.line(&format!("unhandled code 0x{code:x} len={}", body.len()));
            }
        }

        if !did_work {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    drop(tx);
    log_arc.line("=== serve end ===");
    0
}

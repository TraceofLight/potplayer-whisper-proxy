// whisper-proxy: drop-in replacement for whisper.cpp main(64).exe.
//
// Forwards `-f audio.wav` requests to an OpenAI-compatible Whisper server
// (speaches / faster-whisper-server / OpenAI cloud) and emits results in
// the same stdout/SRT/VTT/TXT/JSON formats whisper.cpp produces.
//
// Configuration: whisper-proxy.ini next to the binary, or env vars
// WHISPER_PROXY_URL / WHISPER_PROXY_KEY / WHISPER_PROXY_MODEL / WHISPER_PROXY_TIMEOUT.

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

mod config;
mod whisper_client;
#[cfg(windows)]
mod ipc;

use config::Config;
use whisper_client::{Segment, TranscribeRequest};

// ---------- args ----------

#[derive(Default, Debug)]
struct Args {
    files: Vec<String>,
    language: Option<String>,
    output_prefix: Option<String>,
    out_txt: bool,
    out_srt: bool,
    out_vtt: bool,
    out_json: bool,
    out_json_full: bool,
    out_lrc: bool,
    out_csv: bool,
    offset_ms: i64,
    translate: bool,
    no_timestamps: bool,
    no_prints: bool,
    prompt: Option<String>,
    help: bool,
}

// whisper.cpp options that take a value (everything else: bool flag, no value).
const VALUE_OPTS: &[&str] = &[
    "-t","--threads","-p","--processors","-ot","--offset-t","-on","--offset-n",
    "-d","--duration","-mc","--max-context","-ml","--max-len","-bo","--best-of",
    "-bs","--beam-size","-ac","--audio-ctx","-wt","--word-thold",
    "-et","--entropy-thold","-lpt","--logprob-thold","-nth","--no-speech-thold",
    "-tp","--temperature","-tpi","--temperature-inc","-fp","--font-path",
    "-of","--output-file","-l","--language","--prompt","-m","--model",
    "-f","--file","-oved","--ov-e-device","-dtw","--dtw","-dev","--device",
    "--suppress-regex","--grammar","--grammar-rule","--grammar-penalty",
    "-vm","--vad-model","-vt","--vad-threshold",
    "-vspd","--vad-min-speech-duration-ms","-vsd","--vad-min-silence-duration-ms",
    "-vmsd","--vad-max-speech-duration-s","-vp","--vad-speech-pad-ms",
    "-vo","--vad-samples-overlap",
];

fn opt_takes_value(opt: &str) -> bool {
    VALUE_OPTS.contains(&opt)
}

fn parse_args(raw: Vec<String>) -> Args {
    let mut a = Args::default();
    let mut i = 0;
    while i < raw.len() {
        let arg = &raw[i];
        match arg.as_str() {
            "-h" | "--help" => a.help = true,
            "-f" | "--file" => {
                if let Some(v) = raw.get(i + 1) { a.files.push(v.clone()); i += 1; }
            }
            "-l" | "--language" => {
                if let Some(v) = raw.get(i + 1) { a.language = Some(v.clone()); i += 1; }
            }
            "-of" | "--output-file" => {
                if let Some(v) = raw.get(i + 1) { a.output_prefix = Some(v.clone()); i += 1; }
            }
            "-otxt" | "--output-txt" => a.out_txt = true,
            "-osrt" | "--output-srt" => a.out_srt = true,
            "-ovtt" | "--output-vtt" => a.out_vtt = true,
            "-oj"   | "--output-json" => a.out_json = true,
            "-ojf"  | "--output-json-full" => a.out_json_full = true,
            "-olrc" | "--output-lrc" => a.out_lrc = true,
            "-ocsv" | "--output-csv" => a.out_csv = true,
            "-tr"   | "--translate" => a.translate = true,
            "-nt"   | "--no-timestamps" => a.no_timestamps = true,
            "-np"   | "--no-prints" => a.no_prints = true,
            "-ot"   | "--offset-t" => {
                if let Some(v) = raw.get(i + 1) {
                    a.offset_ms = v.parse().unwrap_or(0);
                    i += 1;
                }
            }
            "--prompt" => {
                if let Some(v) = raw.get(i + 1) { a.prompt = Some(v.clone()); i += 1; }
            }
            other => {
                if other.starts_with('-') {
                    // unknown flag — skip its value if it takes one
                    if opt_takes_value(other) { i += 1; }
                } else {
                    // bare positional — treat as audio file if it exists
                    if Path::new(other).exists() { a.files.push(other.to_string()); }
                }
            }
        }
        i += 1;
    }
    a
}

fn base_dir() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

// ---------- formatting ----------

fn fmt_ts(seconds: f64, comma: bool) -> String {
    let s = if seconds < 0.0 { 0.0 } else { seconds };
    let total_ms = (s * 1000.0).round() as i64;
    let h = total_ms / 3_600_000;
    let m = (total_ms % 3_600_000) / 60_000;
    let sec = (total_ms % 60_000) / 1_000;
    let ms = total_ms % 1_000;
    let sep = if comma { ',' } else { '.' };
    format!("{h:02}:{m:02}:{sec:02}{sep}{ms:03}")
}

// ---------- output writers ----------

fn write_outputs(args: &Args, segs: &[Segment], full_text: &str, lang: &str) -> std::io::Result<()> {
    let Some(prefix) = &args.output_prefix else { return Ok(()); };

    if args.out_txt {
        fs::write(format!("{prefix}.txt"), full_text)?;
    }
    if args.out_srt {
        let mut s = String::new();
        for (i, seg) in segs.iter().enumerate() {
            s.push_str(&format!("{}\n", i + 1));
            s.push_str(&format!("{} --> {}\n", fmt_ts(seg.start, true), fmt_ts(seg.end, true)));
            s.push_str(seg.text.trim());
            s.push_str("\n\n");
        }
        fs::write(format!("{prefix}.srt"), s)?;
    }
    if args.out_vtt {
        let mut s = String::from("WEBVTT\n\n");
        for seg in segs {
            s.push_str(&format!("{} --> {}\n", fmt_ts(seg.start, false), fmt_ts(seg.end, false)));
            s.push_str(seg.text.trim());
            s.push_str("\n\n");
        }
        fs::write(format!("{prefix}.vtt"), s)?;
    }
    if args.out_json || args.out_json_full {
        let mut transcription = Vec::with_capacity(segs.len());
        for seg in segs {
            transcription.push(serde_json::json!({
                "timestamps": {
                    "from": fmt_ts(seg.start, true),
                    "to":   fmt_ts(seg.end,   true),
                },
                "offsets": {
                    "from": (seg.start * 1000.0) as i64,
                    "to":   (seg.end   * 1000.0) as i64,
                },
                "text": seg.text,
            }));
        }
        let payload = serde_json::json!({
            "systeminfo": "whisper-proxy-rs",
            "model": { "type": "remote" },
            "params": { "language": lang, "translate": args.translate },
            "result": { "language": lang },
            "transcription": transcription,
        });
        fs::write(format!("{prefix}.json"), serde_json::to_string_pretty(&payload)?)?;
    }
    if args.out_lrc {
        let mut s = String::new();
        for seg in segs {
            let m = (seg.start as i64) / 60;
            let sec = seg.start - (m as f64) * 60.0;
            s.push_str(&format!("[{m:02}:{sec:05.2}]{}\n", seg.text.trim()));
        }
        fs::write(format!("{prefix}.lrc"), s)?;
    }
    if args.out_csv {
        let mut s = String::from("start,end,text\n");
        for seg in segs {
            let txt = seg.text.replace('"', "\"\"");
            s.push_str(&format!(
                "{},{},\"{}\"\n",
                (seg.start * 1000.0) as i64,
                (seg.end   * 1000.0) as i64,
                txt,
            ));
        }
        fs::write(format!("{prefix}.csv"), s)?;
    }
    Ok(())
}

// ---------- transcription ----------

fn guess_mime(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    if      lower.ends_with(".wav")  { "audio/wav" }
    else if lower.ends_with(".mp3")  { "audio/mpeg" }
    else if lower.ends_with(".ogg")  { "audio/ogg" }
    else if lower.ends_with(".flac") { "audio/flac" }
    else if lower.ends_with(".m4a")  { "audio/mp4" }
    else { "application/octet-stream" }
}

fn transcribe_one(audio_path: &str, args: &Args, cfg: &Config) -> i32 {
    let bytes = match fs::read(audio_path) {
        Ok(b) => b,
        Err(e) => { eprintln!("whisper-proxy: cannot read {audio_path}: {e}"); return 1; }
    };
    let file_name = Path::new(audio_path)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "audio.wav".into());
    let mime = guess_mime(audio_path);

    let req = TranscribeRequest {
        file_name: &file_name,
        file_bytes: &bytes,
        mime,
        language: args.language.as_deref(),
        prompt: args.prompt.as_deref(),
        translate: args.translate,
    };

    let result = match whisper_client::transcribe(cfg, &req) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("whisper-proxy: {e}");
            // HTTP error → 2, request error → 3, body/JSON 파싱 → 4
            return if e.starts_with("HTTP ") { 2 }
                   else if e.starts_with("request failed") { 3 }
                   else { 4 };
        }
    };

    let mut segs = result.segments;
    if args.offset_ms != 0 {
        let off = args.offset_ms as f64 / 1000.0;
        for s in segs.iter_mut() {
            s.start += off;
            s.end   += off;
        }
    }

    let full_text = result.full_text;
    let lang = result
        .language
        .or(args.language.clone())
        .unwrap_or_else(|| "auto".into());

    if !args.no_prints {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        for seg in &segs {
            let line = if args.no_timestamps {
                format!("{}\n", seg.text.trim())
            } else {
                format!("[{} --> {}]  {}\n",
                    fmt_ts(seg.start, false),
                    fmt_ts(seg.end,   false),
                    seg.text.trim())
            };
            let _ = out.write_all(line.as_bytes());
        }
        let _ = out.flush();
    }

    if let Err(e) = write_outputs(args, &segs, &full_text, &lang) {
        eprintln!("whisper-proxy: write output: {e}");
        return 5;
    }
    0
}

fn print_help() {
    println!("whisper-proxy: drop-in replacement for whisper.cpp main(64).exe.");
    println!("Forwards transcription to an OpenAI-compatible Whisper server.");
    println!("Config: whisper-proxy.ini next to the binary, or env vars");
    println!("        WHISPER_PROXY_URL / _KEY / _MODEL / _TIMEOUT.");
}

fn debug_log_path() -> Option<PathBuf> {
    // 디버그 로그가 필요한 두 조건 중 하나:
    //   1) WHISPER_PROXY_DEBUG=1 (또는 0이 아닌 어떤 값)
    //   2) 바이너리와 같은 폴더에 whisper-proxy.debug 파일이 존재
    // 로그 위치는 사용자 권한으로 쓰기 가능한 %TEMP%\whisper-proxy.log
    // (Program Files은 비관리자 프로세스로는 못 쓰므로)
    let dir = base_dir();
    let want = env::var("WHISPER_PROXY_DEBUG").map(|v| v != "0" && !v.is_empty()).unwrap_or(false)
        || dir.join("whisper-proxy.debug").exists();
    if !want { return None; }
    let log_dir = env::var_os("TEMP")
        .or_else(|| env::var_os("TMP"))
        .or_else(|| env::var_os("LOCALAPPDATA"))
        .map(PathBuf::from)
        .unwrap_or(dir);
    Some(log_dir.join("whisper-proxy.log"))
}

fn debug_write(path: &Path, line: &str) {
    use std::fs::OpenOptions;
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
}

fn main() -> ExitCode {
    let raw: Vec<String> = env::args().skip(1).collect();

    // 디버그 로깅: 호출 자체가 있는지 / 어떤 인자로 들어오는지 추적
    let log = debug_log_path();
    if let Some(p) = &log {
        use std::time::{SystemTime, UNIX_EPOCH};
        let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        debug_write(p, &format!("=== call t={t} pid={} ===", std::process::id()));
        debug_write(p, &format!("argv: {:?}", raw));
        debug_write(p, &format!("cwd : {:?}", env::current_dir().ok()));
    }

    // PotPlayer's `-IPC <name>` mode — undocumented protocol over named pipe.
    #[cfg(windows)]
    {
        for w in raw.windows(2) {
            if w[0] == "-IPC" {
                let rc = ipc::run(&w[1]);
                if let Some(p) = &log { debug_write(p, &format!("=== ipc exit rc={rc} ===\n")); }
                return ExitCode::from(rc.max(0).min(255) as u8);
            }
        }
    }

    let args = parse_args(raw);
    if args.help || args.files.is_empty() {
        if let Some(p) = &log { debug_write(p, "no input files -> printed help and exited 0"); }
        print_help();
        return ExitCode::from(0);
    }
    let cfg = config::load(&base_dir());
    if let Some(p) = &log {
        debug_write(p, &format!(
            "config: url={} model={} timeout={}s key_set={}",
            cfg.url, cfg.model, cfg.timeout, !cfg.api_key.is_empty()
        ));
        debug_write(p, &format!("output_prefix={:?} flags(srt={} vtt={} txt={} json={} jsonfull={})",
            args.output_prefix, args.out_srt, args.out_vtt, args.out_txt, args.out_json, args.out_json_full));
    }
    let mut rc: i32 = 0;
    for f in &args.files {
        let r = transcribe_one(f, &args, &cfg);
        if r != 0 { rc = r; }
        if let Some(p) = &log { debug_write(p, &format!("transcribe_one({f}) -> rc={r}")); }
    }
    if let Some(p) = &log { debug_write(p, &format!("=== exit rc={rc} ===\n")); }
    ExitCode::from(rc as u8)
}

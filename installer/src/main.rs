// whisper-proxy-installer
//
// PotPlayer 설치 위치를 자동 탐색해서 Module\Whisper\<engine>\main(64).exe를
// whisper-proxy 래퍼로 교체한다. 원본은 .orig.exe로 백업.
//
// 단일 .exe 안에 main64.exe + whisper-proxy.ini 템플릿을 포함.
// 사용법:
//   whisper-proxy-installer.exe install [--engine Vulkan|CPU|all] [--potplayer-dir PATH] [--url URL] [--model MODEL]
//   whisper-proxy-installer.exe uninstall [--engine Vulkan|CPU|all] [--potplayer-dir PATH]
//   whisper-proxy-installer.exe status [--potplayer-dir PATH]
//
// 빌드 전 proxy를 먼저 빌드해야 한다:
//   cargo build --release -p whisper-proxy
//   cargo build --release -p whisper-proxy-installer

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use winreg::enums::*;
use winreg::RegKey;

// 컴파일 타임에 proxy 바이너리/ini 템플릿을 임베드.
const PROXY_BIN: &[u8] = include_bytes!("../../target/release/main64.exe");
const INI_TEMPLATE: &str = include_str!("../whisper-proxy.ini.template");

// ---------- discovery ----------

fn from_app_paths(view: u32, exe_name: &str) -> Option<PathBuf> {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let key_path = format!("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\{exe_name}");
    let key = hklm
        .open_subkey_with_flags(&key_path, KEY_READ | view)
        .ok()?;
    let path: String = key
        .get_value("Path")
        .ok()
        .or_else(|| key.get_value("").ok())?;
    let p = PathBuf::from(path.trim().trim_matches('"'));
    // App Paths의 Path는 보통 폴더, 아니면 exe → 폴더
    if p.is_dir() {
        Some(p)
    } else {
        p.parent().map(|p| p.to_path_buf())
    }
}

fn discover_potplayer() -> Option<PathBuf> {
    for view in [KEY_WOW64_32KEY, KEY_WOW64_64KEY, 0] {
        for exe in ["PotPlayer.exe", "PotPlayerMini.exe", "PotPlayerMini64.exe"] {
            if let Some(p) = from_app_paths(view, exe) {
                if p.join("Module").join("Whisper").is_dir() {
                    return Some(p);
                }
            }
        }
    }
    let candidates = [
        r"C:\Program Files (x86)\DAUM\PotPlayer",
        r"C:\Program Files\DAUM\PotPlayer",
    ];
    for c in candidates {
        let p = PathBuf::from(c);
        if p.join("Module").join("Whisper").is_dir() {
            return Some(p);
        }
    }
    None
}

fn list_engines(potplayer: &Path) -> Vec<String> {
    let whisper_dir = potplayer.join("Module").join("Whisper");
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(&whisper_dir) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            // engine 폴더는 main64.exe 또는 .orig 백업이 있어야 인정
            let has_main = p.join("main64.exe").exists() || p.join("main64.orig.exe").exists();
            if has_main {
                if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                    out.push(name.to_string());
                }
            }
        }
    }
    out
}

// ---------- write check ----------

fn can_write_to(dir: &Path) -> bool {
    // 실제 대상 폴더에 임시 파일 만들 수 있으면 OK (권한·읽기전용 둘 다 커버)
    let test = dir.join(format!(".whisper-proxy-write-test-{}", std::process::id()));
    match fs::File::create(&test) {
        Ok(f) => {
            drop(f);
            let _ = fs::remove_file(&test);
            true
        }
        Err(_) => false,
    }
}

// ---------- install / uninstall / status ----------

// "<name>.orig.<ext>" 경로 (확장자 없으면 ".orig"만 추가).
fn orig_path(engine_dir: &Path, name: &str) -> PathBuf {
    let p = Path::new(name);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    match p.extension().and_then(|e| e.to_str()) {
        Some(ext) => engine_dir.join(format!("{stem}.orig.{ext}")),
        None => engine_dir.join(format!("{stem}.orig")),
    }
}

fn install_engine(engine_dir: &Path, ini_text: &str) -> std::io::Result<()> {
    for name in ["main64.exe", "main.exe"] {
        let live = engine_dir.join(name);
        let orig = orig_path(engine_dir, name);
        // 백업: 라이브 파일이 우리 바이너리가 아니고, 백업이 아직 없을 때만
        if live.exists() && !orig.exists() {
            // 우리가 이미 설치한 게 아닌지 시그니처 체크
            if !is_our_binary(&live)? {
                fs::copy(&live, &orig)?;
                println!("  backup  {}", orig.display());
            } else {
                println!("  (이미 우리 바이너리, 백업 생략) {}", live.display());
            }
        }
        fs::write(&live, PROXY_BIN)?;
        println!("  install {}", live.display());
    }
    let ini_path = engine_dir.join("whisper-proxy.ini");
    if !ini_path.exists() {
        fs::write(&ini_path, ini_text)?;
        println!("  config  {}", ini_path.display());
    } else {
        println!("  (config 이미 존재, 보존) {}", ini_path.display());
    }
    Ok(())
}

fn is_our_binary(path: &Path) -> std::io::Result<bool> {
    // 임베드한 PROXY_BIN과 동일한 크기 + 동일한 첫 1KB이면 우리 바이너리.
    // (전수 비교 안 하는 이유: 큰 파일도 빠르게 처리)
    let meta = fs::metadata(path)?;
    if meta.len() != PROXY_BIN.len() as u64 {
        return Ok(false);
    }
    let mut head = [0u8; 1024];
    use std::io::Read;
    let mut f = fs::File::open(path)?;
    let n = f.read(&mut head)?;
    let head = &head[..n];
    let need = &PROXY_BIN[..n.min(PROXY_BIN.len())];
    Ok(head == need)
}

fn uninstall_engine(engine_dir: &Path) -> std::io::Result<()> {
    for name in ["main64.exe", "main.exe"] {
        let live = engine_dir.join(name);
        let orig = orig_path(engine_dir, name);
        if orig.exists() {
            // live는 우리 게여야만 덮어씀 (사용자가 수동 변경했을 수 있음)
            if !live.exists() || is_our_binary(&live)? {
                fs::copy(&orig, &live)?;
                fs::remove_file(&orig)?;
                println!("  restore {}", live.display());
            } else {
                println!(
                    "  (현재 파일이 백업이 아님 — 수동 확인 필요) {}",
                    live.display()
                );
            }
        } else {
            println!("  (백업 없음) {}", live.display());
        }
    }
    let ini_path = engine_dir.join("whisper-proxy.ini");
    if ini_path.exists() {
        fs::remove_file(&ini_path)?;
        println!("  remove  {}", ini_path.display());
    }
    Ok(())
}

fn status_engine(engine_dir: &Path) {
    println!("  {}:", engine_dir.display());
    for name in ["main64.exe", "main.exe"] {
        let live = engine_dir.join(name);
        let orig = orig_path(engine_dir, name);
        let ours = live.exists() && is_our_binary(&live).unwrap_or(false);
        let backed_up = orig.exists();
        let mark = if ours && backed_up {
            "INSTALLED (backup OK)"
        } else if ours {
            "INSTALLED (no backup!)"
        } else if backed_up {
            "ORIGINAL ACTIVE (backup present)"
        } else {
            "ORIGINAL ACTIVE"
        };
        println!("    {name:14} -> {mark}");
    }
    let ini_path = engine_dir.join("whisper-proxy.ini");
    if ini_path.exists() {
        match fs::read_to_string(&ini_path) {
            Ok(t) => {
                let url = t
                    .lines()
                    .map(|s| s.trim())
                    .find(|s| s.to_ascii_lowercase().starts_with("url"))
                    .unwrap_or("(url 없음)");
                println!("    config         -> {}", url);
            }
            Err(_) => println!("    config         -> 읽기 실패"),
        }
    } else {
        println!("    config         -> 없음");
    }
}

// ---------- args ----------

struct Cli {
    cmd: String,
    engine: String, // "Vulkan" | "CPU" | "all"
    potplayer_dir: Option<String>,
    url: Option<String>,
    model: Option<String>,
    api_key: Option<String>,
    timeout: Option<String>,
}

impl Default for Cli {
    fn default() -> Self {
        Cli {
            cmd: String::new(),
            engine: "all".into(),
            potplayer_dir: None,
            url: None,
            model: None,
            api_key: None,
            timeout: None,
        }
    }
}

fn parse_cli(args: Vec<String>) -> Cli {
    let mut cli = Cli::default();
    let mut it = args.into_iter();
    if let Some(c) = it.next() {
        cli.cmd = c;
    }
    while let Some(a) = it.next() {
        match a.as_str() {
            "--engine" => {
                cli.engine = it.next().unwrap_or_default();
            }
            "--potplayer-dir" => {
                cli.potplayer_dir = it.next();
            }
            "--url" => {
                cli.url = it.next();
            }
            "--model" => {
                cli.model = it.next();
            }
            "--api-key" => {
                cli.api_key = it.next();
            }
            "--timeout" => {
                cli.timeout = it.next();
            }
            other if other.starts_with("--") => {
                eprintln!("warning: unknown flag '{other}' ignored (typo? — see USAGE)");
            }
            _ => {}
        }
    }
    cli
}

fn render_ini(cli: &Cli) -> String {
    let mut t = INI_TEMPLATE.to_string();
    if let Some(v) = &cli.url {
        t = t.replace("{{URL}}", v);
    } else {
        t = t.replace("{{URL}}", "http://localhost:8000/v1");
    }
    if let Some(v) = &cli.model {
        t = t.replace("{{MODEL}}", v);
    } else {
        t = t.replace("{{MODEL}}", "Systran/faster-whisper-large-v3");
    }
    if let Some(v) = &cli.api_key {
        t = t.replace("{{APIKEY}}", v);
    } else {
        t = t.replace("{{APIKEY}}", "");
    }
    if let Some(v) = &cli.timeout {
        t = t.replace("{{TIMEOUT}}", v);
    } else {
        t = t.replace("{{TIMEOUT}}", "300");
    }
    t
}

fn print_usage() {
    println!("whisper-proxy-installer");
    println!();
    println!("USAGE:");
    println!("  install   [--engine Vulkan|CPU|all] [--potplayer-dir PATH]");
    println!("            [--url URL] [--model MODEL] [--api-key KEY] [--timeout SEC]");
    println!("  uninstall [--engine Vulkan|CPU|all] [--potplayer-dir PATH]");
    println!("  status    [--potplayer-dir PATH]");
    println!("  detect");
}

// ---------- main ----------

fn main() -> ExitCode {
    let raw: Vec<String> = env::args().skip(1).collect();
    if raw.is_empty() {
        print_usage();
        return ExitCode::from(0);
    }
    let cli = parse_cli(raw);

    let potplayer = match &cli.potplayer_dir {
        Some(p) => PathBuf::from(p),
        None => match discover_potplayer() {
            Some(p) => p,
            None => {
                eprintln!("error: PotPlayer 설치 위치를 찾을 수 없음.");
                eprintln!("       --potplayer-dir 로 직접 지정하세요.");
                return ExitCode::from(2);
            }
        },
    };

    if !potplayer.join("Module").join("Whisper").is_dir() {
        eprintln!(
            "error: {} 안에 Module\\Whisper 폴더가 없음. PotPlayer가 맞는지 확인.",
            potplayer.display()
        );
        return ExitCode::from(2);
    }
    println!("PotPlayer: {}", potplayer.display());

    let engines_all = list_engines(&potplayer);
    if engines_all.is_empty() {
        eprintln!("error: 인식 가능한 Whisper 엔진 폴더가 없음 (Module\\Whisper\\)");
        return ExitCode::from(2);
    }

    let engines: Vec<String> = match cli.engine.to_ascii_lowercase().as_str() {
        "all" => engines_all.clone(),
        e if engines_all.iter().any(|x| x.eq_ignore_ascii_case(e)) => engines_all
            .into_iter()
            .filter(|x| x.eq_ignore_ascii_case(e))
            .collect(),
        _ => {
            eprintln!(
                "error: '--engine {}' 알 수 없음. 사용 가능: {}",
                cli.engine,
                engines_all.join(", ")
            );
            return ExitCode::from(2);
        }
    };
    println!("대상 엔진: {}", engines.join(", "));

    match cli.cmd.as_str() {
        "detect" => {
            println!("(자동 탐색 결과만 출력하고 종료)");
            ExitCode::from(0)
        }
        "status" => {
            for e in &engines {
                status_engine(&potplayer.join("Module").join("Whisper").join(e));
            }
            ExitCode::from(0)
        }
        "install" => {
            // 첫 번째 엔진 폴더로 쓰기 가능 여부 점검 (대표 샘플)
            let probe = potplayer.join("Module").join("Whisper").join(&engines[0]);
            if !can_write_to(&probe) {
                eprintln!(
                    "error: {} 폴더에 쓰기 불가. 관리자 PowerShell에서 재실행하거나",
                    probe.display()
                );
                eprintln!("       --potplayer-dir 로 쓰기 가능한 경로 지정.");
                return ExitCode::from(3);
            }
            let ini_text = render_ini(&cli);
            for e in &engines {
                println!("[{e}]");
                let dir = potplayer.join("Module").join("Whisper").join(e);
                if let Err(err) = install_engine(&dir, &ini_text) {
                    eprintln!("  install 실패: {err}");
                    return ExitCode::from(4);
                }
            }
            println!();
            println!("완료. PotPlayer F5 -> 자막 -> 실시간 자막 변환에서 엔진 선택 후 사용.");
            println!("  ini 위치: Module\\Whisper\\<engine>\\whisper-proxy.ini");
            ExitCode::from(0)
        }
        "uninstall" => {
            let probe = potplayer.join("Module").join("Whisper").join(&engines[0]);
            if !can_write_to(&probe) {
                eprintln!(
                    "error: {} 폴더에 쓰기 불가. 관리자 PowerShell에서 재실행.",
                    probe.display()
                );
                return ExitCode::from(3);
            }
            for e in &engines {
                println!("[{e}]");
                let dir = potplayer.join("Module").join("Whisper").join(e);
                if let Err(err) = uninstall_engine(&dir) {
                    eprintln!("  uninstall 실패: {err}");
                    return ExitCode::from(4);
                }
            }
            println!();
            println!("롤백 완료.");
            ExitCode::from(0)
        }
        _ => {
            print_usage();
            ExitCode::from(1)
        }
    }
}

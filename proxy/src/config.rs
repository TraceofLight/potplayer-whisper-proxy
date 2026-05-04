// 공통 설정 로더 — main.rs(`-IPC` 외 모드)와 ipc.rs(`-IPC` 모드) 양쪽이 사용.
//
// 우선순위: 환경변수 > whisper-proxy.ini > 내장 default.
// INI는 `whisper-proxy.ini`(바이너리와 같은 폴더)를 읽으며, [server] 섹션의
// url / api_key / model / timeout 키만 인식.
//
// PowerShell `Set-Content -Encoding UTF8`이 파일 앞에 붙이는 UTF-8 BOM은
// 자동으로 제거 (예전 버전에서 첫 키가 깨지는 버그가 있었음).

use std::env;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Config {
    pub url: String,
    pub api_key: String,
    pub model: String,
    pub timeout: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            url: "http://localhost:8000/v1".to_string(),
            api_key: String::new(),
            model: "Systran/faster-whisper-large-v3".to_string(),
            timeout: 120,
        }
    }
}

fn parse_ini(text: &str, cfg: &mut Config) {
    let text = text.trim_start_matches('\u{feff}');
    let mut in_server = false;
    for raw in text.lines() {
        let line = raw.trim().trim_start_matches('\u{feff}');
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            in_server = line.eq_ignore_ascii_case("[server]");
            continue;
        }
        if !in_server {
            continue;
        }
        let Some(eq) = line.find('=') else {
            continue;
        };
        let k = line[..eq].trim().to_ascii_lowercase();
        let v = line[eq + 1..].trim().to_string();
        match k.as_str() {
            "url" => cfg.url = v,
            "api_key" => cfg.api_key = v,
            "model" => cfg.model = v,
            "timeout" => {
                if let Ok(n) = v.parse() {
                    cfg.timeout = n;
                }
            }
            _ => {}
        }
    }
}

pub fn load(base_dir: &Path) -> Config {
    let mut cfg = Config::default();
    let ini = base_dir.join("whisper-proxy.ini");
    if let Ok(text) = fs::read_to_string(&ini) {
        parse_ini(&text, &mut cfg);
    }
    if let Ok(v) = env::var("WHISPER_PROXY_URL") {
        cfg.url = v;
    }
    if let Ok(v) = env::var("WHISPER_PROXY_KEY") {
        cfg.api_key = v;
    }
    if let Ok(v) = env::var("WHISPER_PROXY_MODEL") {
        cfg.model = v;
    }
    if let Ok(v) = env::var("WHISPER_PROXY_TIMEOUT") {
        if let Ok(n) = v.parse() {
            cfg.timeout = n;
        }
    }
    cfg
}

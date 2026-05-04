// 공통 Whisper API 클라이언트 — main.rs(파일 기반 호출)와
// ipc.rs(실시간 chunk 호출) 양쪽이 사용.
//
// OpenAI 호환 multipart/form-data 요청을 만들고 응답 JSON에서
// segments / text / language를 파싱해 돌려준다.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::config::Config;

#[derive(Debug, Clone)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

pub struct TranscribeResult {
    pub segments: Vec<Segment>,
    pub full_text: String,
    pub language: Option<String>,
}

pub struct TranscribeRequest<'a> {
    pub file_name: &'a str,
    pub file_bytes: &'a [u8],
    pub mime: &'a str,
    pub language: Option<&'a str>,
    pub prompt: Option<&'a str>,
    pub translate: bool,
}

pub fn transcribe(cfg: &Config, req: &TranscribeRequest<'_>) -> Result<TranscribeResult, String> {
    let boundary = make_boundary();

    let mut fields: Vec<(&str, &str)> = vec![
        ("model", cfg.model.as_str()),
        ("response_format", "verbose_json"),
    ];
    if let Some(l) = req.language {
        if l != "auto" && !l.is_empty() {
            fields.push(("language", l));
        }
    }
    if let Some(p) = req.prompt {
        fields.push(("prompt", p));
    }

    let body = build_multipart(&boundary, &fields, req.file_name, req.file_bytes, req.mime);

    let endpoint_path = if req.translate {
        "/audio/translations"
    } else {
        "/audio/transcriptions"
    };
    let endpoint = format!("{}{}", cfg.url.trim_end_matches('/'), endpoint_path);

    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(cfg.timeout))
        .build();
    let mut http_req = agent.post(&endpoint).set(
        "Content-Type",
        &format!("multipart/form-data; boundary={boundary}"),
    );
    if !cfg.api_key.is_empty() {
        http_req = http_req.set("Authorization", &format!("Bearer {}", cfg.api_key));
    }

    let resp = http_req.send_bytes(&body).map_err(|e| match e {
        ureq::Error::Status(code, r) => {
            let body = r.into_string().unwrap_or_default();
            format!("HTTP {code}: {body}")
        }
        e => format!("request failed: {e}"),
    })?;

    let resp_text = resp.into_string().map_err(|e| format!("read body: {e}"))?;
    let v: Value = serde_json::from_str(&resp_text).map_err(|e| {
        format!(
            "non-JSON response: {e} -- {}",
            resp_text.chars().take(200).collect::<String>()
        )
    })?;

    let mut segments: Vec<Segment> = Vec::new();
    if let Some(arr) = v.get("segments").and_then(|s| s.as_array()) {
        for seg in arr {
            let start = seg.get("start").and_then(|x| x.as_f64()).unwrap_or(0.0);
            let end = seg.get("end").and_then(|x| x.as_f64()).unwrap_or(0.0);
            let text = seg
                .get("text")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            segments.push(Segment { start, end, text });
        }
    }
    if segments.is_empty() {
        if let Some(t) = v.get("text").and_then(|x| x.as_str()) {
            if !t.is_empty() {
                segments.push(Segment {
                    start: 0.0,
                    end: 0.0,
                    text: t.to_string(),
                });
            }
        }
    }

    let full_text = v
        .get("text")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            segments
                .iter()
                .map(|s| s.text.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        });
    let language = v
        .get("language")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());

    Ok(TranscribeResult {
        segments,
        full_text,
        language,
    })
}

fn make_boundary() -> String {
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    format!("----whisperproxy{ns:032x}{pid:08x}")
}

fn build_multipart(
    boundary: &str,
    fields: &[(&str, &str)],
    file_name: &str,
    file_bytes: &[u8],
    mime: &str,
) -> Vec<u8> {
    let mut body = Vec::with_capacity(file_bytes.len() + 1024);
    for (k, v) in fields {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{k}\"\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(v.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {mime}\r\n\r\n").as_bytes());
    body.extend_from_slice(file_bytes);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

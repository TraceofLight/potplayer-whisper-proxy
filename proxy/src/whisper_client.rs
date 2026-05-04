// OpenAI 호환 Whisper API 클라이언트 (multipart/form-data POST).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::config::Config;

// 영어 이름("korean") → ISO 639-1("ko") 변환. 이미 ISO 코드면 그대로 통과.
// 매핑 테이블은 whisper.cpp의 g_lang(99개) 기준.
fn normalize_language(lang: &str) -> String {
    let lower = lang.to_ascii_lowercase();
    // 짧고 ASCII 문자만으로 구성됐으면 이미 ISO 639-1/2 코드로 간주
    if lower.len() <= 3 && lower.chars().all(|c| c.is_ascii_alphabetic()) {
        return lower;
    }
    let iso = match lower.as_str() {
        "english" => "en",
        "chinese" => "zh",
        "german" => "de",
        "spanish" => "es",
        "russian" => "ru",
        "korean" => "ko",
        "french" => "fr",
        "japanese" => "ja",
        "portuguese" => "pt",
        "turkish" => "tr",
        "polish" => "pl",
        "catalan" => "ca",
        "dutch" => "nl",
        "arabic" => "ar",
        "swedish" => "sv",
        "italian" => "it",
        "indonesian" => "id",
        "hindi" => "hi",
        "finnish" => "fi",
        "vietnamese" => "vi",
        "hebrew" => "he",
        "ukrainian" => "uk",
        "greek" => "el",
        "malay" => "ms",
        "czech" => "cs",
        "romanian" => "ro",
        "danish" => "da",
        "hungarian" => "hu",
        "tamil" => "ta",
        "norwegian" => "no",
        "thai" => "th",
        "urdu" => "ur",
        "croatian" => "hr",
        "bulgarian" => "bg",
        "lithuanian" => "lt",
        "latin" => "la",
        "maori" => "mi",
        "malayalam" => "ml",
        "welsh" => "cy",
        "slovak" => "sk",
        "telugu" => "te",
        "persian" => "fa",
        "latvian" => "lv",
        "bengali" => "bn",
        "serbian" => "sr",
        "azerbaijani" => "az",
        "slovenian" => "sl",
        "kannada" => "kn",
        "estonian" => "et",
        "macedonian" => "mk",
        "breton" => "br",
        "basque" => "eu",
        "icelandic" => "is",
        "armenian" => "hy",
        "nepali" => "ne",
        "mongolian" => "mn",
        "bosnian" => "bs",
        "kazakh" => "kk",
        "albanian" => "sq",
        "swahili" => "sw",
        "galician" => "gl",
        "marathi" => "mr",
        "punjabi" => "pa",
        "sinhala" => "si",
        "khmer" => "km",
        "shona" => "sn",
        "yoruba" => "yo",
        "somali" => "so",
        "afrikaans" => "af",
        "occitan" => "oc",
        "georgian" => "ka",
        "belarusian" => "be",
        "tajik" => "tg",
        "sindhi" => "sd",
        "gujarati" => "gu",
        "amharic" => "am",
        "yiddish" => "yi",
        "lao" => "lo",
        "uzbek" => "uz",
        "faroese" => "fo",
        "haitian creole" => "ht",
        "pashto" => "ps",
        "turkmen" => "tk",
        "nynorsk" => "nn",
        "maltese" => "mt",
        "sanskrit" => "sa",
        "luxembourgish" => "lb",
        "myanmar" => "my",
        "tibetan" => "bo",
        "tagalog" => "tl",
        "malagasy" => "mg",
        "assamese" => "as",
        "tatar" => "tt",
        "hawaiian" => "haw",
        "lingala" => "ln",
        "hausa" => "ha",
        "bashkir" => "ba",
        "javanese" => "jw",
        "sundanese" => "su",
        "cantonese" => "yue",
        _ => return lower, // 알 수 없으면 그대로 (server가 reject 시 명확한 에러)
    };
    iso.to_string()
}

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
    // ISO 639-1로 정규화. fields가 borrow하므로 함수 끝까지 살아야 함.
    let normalized = req
        .language
        .filter(|l| !l.is_empty() && *l != "auto")
        .map(normalize_language);
    if let Some(ref n) = normalized {
        fields.push(("language", n.as_str()));
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
    if segments.is_empty()
        && let Some(t) = v.get("text").and_then(|x| x.as_str())
        && !t.is_empty()
    {
        segments.push(Segment {
            start: 0.0,
            end: 0.0,
            text: t.to_string(),
        });
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

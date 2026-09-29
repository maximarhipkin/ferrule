//! M41 §2: voice and audio messages turned into text before the agent sees
//! them. The router calls a [`Transcriber`] for every `audio/*` attachment an
//! adapter saved; ferrule makes the call, so the key never enters the
//! sandbox. Two backends: an OpenAI-compatible `/audio/transcriptions`
//! endpoint and a local command. Neither assumes a language: one is sent
//! only when the config pins it.

use crate::channels::files::multipart;
use async_trait::async_trait;
use ferrule_core::ledger::{LedgerRecord, LedgerSink};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The ledger's `call_kind` and `provider` for a transcription.
pub const TRANSCRIPTION_CALL_KIND: &str = "transcription";

/// What was heard.
#[derive(Debug, Clone, PartialEq)]
pub struct Heard {
    pub text: String,
    /// The audio's length, when the backend or the file says.
    pub seconds: Option<f64>,
    /// The language the backend detected, if it said.
    pub language: Option<String>,
}

/// One audio file to transcribe, and whose it is (for the ledger).
#[derive(Debug, Clone)]
pub struct Audio<'a> {
    pub path: &'a Path,
    pub mime: &'a str,
    pub session_id: &'a str,
    pub channel: &'a str,
}

#[async_trait]
pub trait Transcriber: Send + Sync {
    /// `Err` is the reason in plain words, for the person and the agent.
    async fn transcribe(&self, audio: &Audio<'_>) -> Result<Heard, String>;
}

/// What the router does with audio.
#[derive(Clone)]
pub enum Transcription {
    On(Arc<dyn Transcriber>),
    /// Off: the agent is told so, and each chat once hears `how` (how to
    /// turn it on).
    Off {
        how: String,
    },
}

/// Where each call's ledger row goes, and what it costs.
#[derive(Clone, Default)]
pub struct Ledger {
    pub sink: Option<Arc<dyn LedgerSink>>,
    /// USD per minute of audio; `None` or 0 for a free backend.
    pub price_per_minute: Option<f64>,
}

impl Ledger {
    fn record(
        &self,
        audio: &Audio<'_>,
        model: &str,
        started: Instant,
        result: &Result<Heard, String>,
    ) {
        let Some(sink) = &self.sink else {
            return;
        };
        let (outcome, error_kind, error_message, cost_usd) = match result {
            Ok(h) => (
                "ok",
                None,
                None,
                h.seconds
                    .map(|s| s / 60.0 * self.price_per_minute.unwrap_or(0.0)),
            ),
            Err(e) => (
                "error",
                Some(TRANSCRIPTION_CALL_KIND.to_string()),
                Some(e.clone()),
                None,
            ),
        };
        sink.record(LedgerRecord {
            timestamp: chrono::Utc::now().to_rfc3339(),
            session_id: audio.session_id.to_string(),
            task_shape: "gateway".into(),
            origin: Some(audio.channel.to_string()),
            provider: TRANSCRIPTION_CALL_KIND.into(),
            model: model.to_string(),
            iteration: 0,
            call_kind: TRANSCRIPTION_CALL_KIND.into(),
            input_tokens: 0,
            cached_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens: 0,
            tool_calls: 0,
            latency_ms: started.elapsed().as_millis() as u64,
            outcome: outcome.into(),
            error_kind,
            error_message,
            cost_usd,
            eval: None,
            tree: None,
            route: None,
            speed: None,
            plan: None,
            notional_usd: None,
        });
    }
}

/// An OpenAI-compatible `POST {base_url}/audio/transcriptions` (OpenAI,
/// Groq, a local whisper server).
pub struct OpenAiTranscriber {
    pub base_url: String,
    /// The env var holding the key; read on every call, as provider calls
    /// read theirs. `None`: no `Authorization` header (a local server).
    pub key_env: Option<String>,
    pub model: String,
    /// Pinned language (ISO 639-1); `None` lets the backend detect it.
    pub language: Option<String>,
    pub timeout: Duration,
    pub ledger: Ledger,
    pub client: reqwest::Client,
}

impl OpenAiTranscriber {
    async fn post(
        &self,
        name: &str,
        mime: &str,
        bytes: &[u8],
        format: &str,
    ) -> Result<(reqwest::StatusCode, String), String> {
        let mut fields = vec![("model", self.model.as_str()), ("response_format", format)];
        if let Some(lang) = &self.language {
            fields.push(("language", lang.as_str()));
        }
        let (boundary, body) = multipart(&fields, ("file", name, mime, bytes));
        let url = format!(
            "{}/audio/transcriptions",
            self.base_url.trim_end_matches('/')
        );
        let mut req = self
            .client
            .post(&url)
            .timeout(self.timeout)
            .header(
                "Content-Type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body);
        if let Some(var) = &self.key_env {
            let key = std::env::var(var)
                .ok()
                .filter(|k| !k.is_empty())
                .ok_or_else(|| {
                    format!("{var} isn't set, so there's no key for the transcription service")
                })?;
            req = req.bearer_auth(key);
        }
        let resp = req.send().await.map_err(|e| {
            if e.is_timeout() {
                format!(
                    "the transcription service didn't answer within {} s",
                    self.timeout.as_secs()
                )
            } else {
                format!(
                    "couldn't reach the transcription service at {}: {}",
                    self.base_url,
                    root_cause(&e)
                )
            }
        })?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        Ok((status, text))
    }

    async fn call(&self, audio: &Audio<'_>) -> Result<Heard, String> {
        let bytes = std::fs::read(audio.path)
            .map_err(|e| format!("couldn't read the saved audio file: {e}"))?;
        let name = upload_name(audio.path, audio.mime);
        let (mut status, mut body) = self.post(&name, audio.mime, &bytes, "verbose_json").await?;
        // Some compatible servers take only `json` (or `text`).
        if status.as_u16() == 400 && mentions(&body, &["response_format", "verbose_json"]) {
            (status, body) = self.post(&name, audio.mime, &bytes, "json").await?;
        }
        if !status.is_success() {
            return Err(refusal(status.as_u16(), &body, audio.mime, &self.key_env));
        }
        let heard = match serde_json::from_str::<Value>(&body) {
            Ok(v) => Heard {
                text: v["text"].as_str().unwrap_or("").trim().to_string(),
                seconds: v["duration"].as_f64(),
                language: v["language"].as_str().map(str::to_string),
            },
            // A server asked for json that answered plain text anyway.
            Err(_) => Heard {
                text: body.trim().to_string(),
                seconds: None,
                language: None,
            },
        };
        Ok(heard)
    }
}

#[async_trait]
impl Transcriber for OpenAiTranscriber {
    async fn transcribe(&self, audio: &Audio<'_>) -> Result<Heard, String> {
        let started = Instant::now();
        let mut result = self.call(audio).await;
        if let Ok(h) = &mut result {
            if h.seconds.is_none() {
                h.seconds = file_seconds(audio.path, audio.mime);
            }
        }
        self.ledger.record(audio, &self.model, started, &result);
        result
    }
}

/// A local program: `{file}` in the template is the audio file's path (one
/// argument however it's spelled), there's no shell, and stdout is the
/// transcript. It's killed at the timeout.
pub struct CommandTranscriber {
    pub template: String,
    pub timeout: Duration,
    pub ledger: Ledger,
}

impl CommandTranscriber {
    async fn call(&self, audio: &Audio<'_>) -> Result<Heard, String> {
        let file = audio.path.to_string_lossy();
        let args: Vec<String> = split_command(&self.template)?
            .into_iter()
            .map(|a| expand_home(&a.replace("{file}", &file)))
            .collect();
        let Some((program, rest)) = args.split_first() else {
            return Err("[transcription] command is empty".into());
        };
        let child = tokio::process::Command::new(program)
            .args(rest)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("couldn't start the transcription command `{program}`: {e}"))?;
        let out = match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Ok(out) => out.map_err(|e| format!("the transcription command failed: {e}"))?,
            Err(_) => {
                return Err(format!(
                    "the transcription command didn't finish within {} s (timeout_secs)",
                    self.timeout.as_secs()
                ))
            }
        };
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let last = stderr
                .lines()
                .rev()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim();
            return Err(format!(
                "the transcription command failed ({}){}",
                out.status,
                if last.is_empty() {
                    String::new()
                } else {
                    format!(": {last}")
                }
            ));
        }
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if text.is_empty() {
            return Err("the transcription command printed nothing".into());
        }
        Ok(Heard {
            text,
            seconds: file_seconds(audio.path, audio.mime),
            language: None,
        })
    }
}

#[async_trait]
impl Transcriber for CommandTranscriber {
    async fn transcribe(&self, audio: &Audio<'_>) -> Result<Heard, String> {
        let started = Instant::now();
        let result = self.call(audio).await;
        self.ledger.record(audio, "command", started, &result);
        result
    }
}

/// The innermost error, which says what actually failed ("Connection
/// refused") where reqwest's own says "error sending request".
fn root_cause(e: &reqwest::Error) -> String {
    let mut last: &dyn std::error::Error = e;
    while let Some(next) = last.source() {
        last = next;
    }
    last.to_string()
}

/// The name the file goes up as. OpenAI tells formats by the extension and
/// doesn't know `.oga`/`.opus` (Telegram's and WhatsApp's voice notes),
/// which are Ogg, so they go up as `.ogg`.
fn upload_name(path: &Path, mime: &str) -> String {
    let name = path
        .file_name()
        .map_or_else(|| "audio".into(), |n| n.to_string_lossy().into_owned());
    let stem = Path::new(&name)
        .file_stem()
        .map_or_else(|| "audio".into(), |s| s.to_string_lossy().into_owned());
    let ext = Path::new(&name)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase());
    match ext.as_deref() {
        Some("oga" | "opus") => format!("{stem}.ogg"),
        Some(_) => name,
        None if mime.contains("ogg") || mime.contains("opus") => format!("{stem}.ogg"),
        None => match mime {
            "audio/mpeg" => format!("{stem}.mp3"),
            "audio/mp4" | "audio/x-m4a" | "audio/m4a" => format!("{stem}.m4a"),
            "audio/wav" | "audio/x-wav" => format!("{stem}.wav"),
            "audio/webm" => format!("{stem}.webm"),
            "audio/flac" => format!("{stem}.flac"),
            _ => name,
        },
    }
}

fn mentions(body: &str, words: &[&str]) -> bool {
    let lower = body.to_ascii_lowercase();
    words.iter().any(|w| lower.contains(w))
}

/// A refused request in words.
fn refusal(status: u16, body: &str, mime: &str, key_env: &Option<String>) -> String {
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v["error"]["message"]
                .as_str()
                .or_else(|| v["error"].as_str())
                .or_else(|| v["detail"].as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.chars().take(300).collect::<String>())
        .trim()
        .to_string();
    let key = key_env.as_deref().unwrap_or("its key variable");
    match status {
        401 | 403 => {
            format!("the transcription service refused the key in {key} (HTTP {status}): {message}")
        }
        429 => format!(
            "the transcription service is rate limiting or out of credit (HTTP 429): {message}"
        ),
        400 | 415 if mentions(&message, &["format", "decode", "unsupported", "codec"]) => {
            format!("the transcription service didn't accept this audio format ({mime}): {message}")
        }
        _ => format!("the transcription service answered HTTP {status}: {message}"),
    }
}

/// Splits a command template into arguments: whitespace separates, single
/// and double quotes group, no other shell syntax.
pub fn split_command(template: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut in_arg = false;
    let mut quote: Option<char> = None;
    for c in template.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                in_arg = true;
            }
            (None, c) if c.is_whitespace() => {
                if in_arg {
                    args.push(std::mem::take(&mut cur));
                    in_arg = false;
                }
            }
            (None, c) => {
                cur.push(c);
                in_arg = true;
            }
        }
    }
    if quote.is_some() {
        return Err("[transcription] command has an unclosed quote".into());
    }
    if in_arg {
        args.push(cur);
    }
    Ok(args)
}

/// `~/x` → `$HOME/x` (there's no shell to do it).
fn expand_home(arg: &str) -> String {
    match (arg.strip_prefix("~/"), home()) {
        (Some(rest), Some(home)) => home.join(rest).to_string_lossy().into_owned(),
        _ => arg.to_string(),
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// An Ogg Opus file's length, read from its pages (a voice note's usual
/// form); `None` for anything else.
pub fn file_seconds(path: &Path, mime: &str) -> Option<f64> {
    let ogg = mime.contains("ogg")
        || mime.contains("opus")
        || path.extension().is_some_and(|e| {
            matches!(
                e.to_string_lossy().to_ascii_lowercase().as_str(),
                "ogg" | "oga" | "opus"
            )
        });
    if !ogg {
        return None;
    }
    opus_seconds(&std::fs::read(path).ok()?)
}

/// The last Ogg page's granule position, less the Opus pre-skip, at
/// Opus's fixed 48 kHz.
pub fn opus_seconds(bytes: &[u8]) -> Option<f64> {
    let head = find(bytes, b"OpusHead", 0)?;
    let pre_skip = u16::from_le_bytes(bytes.get(head + 10..head + 12)?.try_into().ok()?);
    let mut last = None;
    let mut at = 0;
    while let Some(i) = find(bytes, b"OggS", at) {
        last = Some(i);
        at = i + 4;
    }
    let page = last?;
    let granule = i64::from_le_bytes(bytes.get(page + 6..page + 14)?.try_into().ok()?);
    if granule <= 0 {
        return None;
    }
    Some((granule - i64::from(pre_skip)).max(0) as f64 / 48_000.0)
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    hay.get(from..)?
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| i + from)
}

/// `0:14`, `12:03`, `1:02:03`.
pub fn clock(seconds: f64) -> String {
    let s = seconds.round().max(0.0) as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// The line the agent reads for a transcribed voice message.
pub fn heard_line(h: &Heard) -> String {
    match h.seconds {
        Some(s) => format!("[voice message, {}, transcribed]: {}", clock(s), h.text),
        None => format!("[voice message, transcribed]: {}", h.text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal Ogg Opus stream: an `OpusHead` page and a last page.
    fn ogg(pre_skip: u16, granule: i64) -> Vec<u8> {
        let mut b = b"OggS".to_vec();
        b.extend_from_slice(&[0; 24]);
        b.extend_from_slice(b"OpusHead");
        b.push(1); // version
        b.push(1); // channels
        b.extend_from_slice(&pre_skip.to_le_bytes());
        b.extend_from_slice(&[0; 12]);
        b.extend_from_slice(b"OggS");
        b.push(0);
        b.push(4);
        b.extend_from_slice(&granule.to_le_bytes());
        b.extend_from_slice(&[0; 16]);
        b
    }

    #[test]
    fn a_voice_notes_length_is_read_from_its_last_page() {
        let s = opus_seconds(&ogg(312, 312 + 48_000 * 14)).unwrap();
        assert!((s - 14.0).abs() < 0.001, "{s}");
        assert_eq!(opus_seconds(b"not audio"), None);
    }

    #[test]
    fn clocks_and_lines() {
        assert_eq!(clock(14.2), "0:14");
        assert_eq!(clock(723.0), "12:03");
        assert_eq!(clock(3723.0), "1:02:03");
        let h = Heard {
            text: "בוא נקבע".into(),
            seconds: Some(14.0),
            language: Some("hebrew".into()),
        };
        assert_eq!(
            heard_line(&h),
            "[voice message, 0:14, transcribed]: בוא נקבע"
        );
    }

    #[test]
    fn templates_split_like_a_shell_would_without_one() {
        assert_eq!(
            split_command(r#"whisper-cli -m "/my models/x.bin" -otxt '{file}'"#).unwrap(),
            ["whisper-cli", "-m", "/my models/x.bin", "-otxt", "{file}"]
        );
        assert!(split_command("a \"b").is_err());
    }

    #[test]
    fn voice_notes_go_up_as_ogg() {
        assert_eq!(
            upload_name(Path::new("/x/12-voice.oga"), "audio/ogg"),
            "12-voice.ogg"
        );
        assert_eq!(upload_name(Path::new("/x/a.mp3"), "audio/mpeg"), "a.mp3");
        assert_eq!(upload_name(Path::new("/x/memo"), "audio/ogg"), "memo.ogg");
    }

    #[test]
    fn a_rejected_format_is_said_plainly() {
        let body =
            r#"{"error":{"message":"Invalid file format. Supported formats: ['flac', 'mp3']"}}"#;
        let why = refusal(400, body, "audio/amr", &Some("OPENAI_API_KEY".into()));
        assert!(
            why.starts_with("the transcription service didn't accept this audio format (audio/amr): Invalid file format"),
            "{why}"
        );
        let why = refusal(
            401,
            r#"{"error":{"message":"bad key"}}"#,
            "audio/ogg",
            &Some("OPENAI_API_KEY".into()),
        );
        assert!(why.contains("refused the key in OPENAI_API_KEY"), "{why}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_command_backend_reads_stdout_and_is_killed_at_the_timeout() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("v.ogg");
        std::fs::write(&f, ogg(0, 48_000 * 3)).unwrap();
        let audio = Audio {
            path: &f,
            mime: "audio/ogg",
            session_id: "s",
            channel: "telegram",
        };
        let t = CommandTranscriber {
            template: "echo heard {file}".into(),
            timeout: Duration::from_secs(10),
            ledger: Ledger::default(),
        };
        let h = t.transcribe(&audio).await.unwrap();
        assert_eq!(h.text, format!("heard {}", f.display()));
        assert_eq!(h.seconds, Some(3.0));
        let slow = CommandTranscriber {
            template: "sleep 30".into(),
            timeout: Duration::from_millis(200),
            ledger: Ledger::default(),
        };
        let started = Instant::now();
        let err = slow.transcribe(&audio).await.unwrap_err();
        assert!(err.contains("didn't finish within"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}

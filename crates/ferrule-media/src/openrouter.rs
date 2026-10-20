//! OpenRouter's media endpoints: `POST {base}/images/generations`
//! (OpenAI-compatible, sync) and `POST {base}/videos` + `GET
//! {base}/videos/{id}` (async jobs). Reached through ferrule's credential
//! proxy: the caller hands over the proxy's placeholder key and an egress
//! client from `ferrule_tools::egress::client_builder`, never a real key.

use crate::{ImageOut, ImageReq, MediaBackend, MediaError, VideoJob, VideoReq, VideoState};
use base64::Engine as _;
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct MediaSettings {
    /// Up to and including `/v1` (OpenRouter: `https://openrouter.ai/api/v1`).
    pub base_url: String,
    pub image_model: String,
    pub video_model: String,
    /// The proxy's placeholder for the key, sent as `Authorization: Bearer`.
    pub key: Option<String>,
    /// The key's env var, named in a 401's message.
    pub key_env: Option<String>,
    pub timeout: Duration,
}

pub struct OpenRouterMedia {
    settings: MediaSettings,
    client: reqwest::Client,
    host: String,
}

impl OpenRouterMedia {
    pub fn new(settings: MediaSettings, client: reqwest::Client) -> Self {
        let host = reqwest::Url::parse(&settings.base_url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| settings.base_url.clone());
        Self {
            settings,
            client,
            host,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.settings.base_url.trim_end_matches('/'))
    }

    fn post(&self, url: String, body: &Value) -> reqwest::RequestBuilder {
        let mut req = self
            .client
            .post(url)
            .timeout(self.settings.timeout)
            .json(body);
        if let Some(key) = &self.settings.key {
            req = req.bearer_auth(key);
        }
        req
    }

    async fn checked(&self, req: reqwest::RequestBuilder) -> Result<Value, MediaError> {
        let transport = |e: reqwest::Error| MediaError::Transport {
            host: self.host.clone(),
            message: if e.is_timeout() {
                format!("no answer within {}s", self.settings.timeout.as_secs())
            } else {
                e.without_url().to_string()
            },
        };
        let resp = req.send().await.map_err(transport)?;
        let status = resp.status();
        if status == 401 || status == 403 {
            return Err(MediaError::Unauthorized {
                host: self.host.clone(),
                status: status.as_u16(),
                key_env: self
                    .settings
                    .key_env
                    .clone()
                    .unwrap_or_else(|| "the key".into()),
            });
        }
        if status == 429 {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<f64>().ok())
                .map(|s| s.ceil().max(0.0) as u64);
            return Err(MediaError::RateLimited {
                host: self.host.clone(),
                retry_after,
            });
        }
        let bytes = resp.bytes().await.map_err(transport)?;
        if !status.is_success() {
            let body: String = String::from_utf8_lossy(&bytes).chars().take(300).collect();
            return Err(MediaError::Http {
                host: self.host.clone(),
                status: status.as_u16(),
                body,
            });
        }
        serde_json::from_slice(&bytes)
            .map_err(|e| MediaError::BadAnswer(format!("not a JSON answer: {e}")))
    }

    /// A workspace reference image as a data URL for the request.
    fn data_url(path: &Path) -> Result<String, MediaError> {
        let bytes = std::fs::read(path)
            .map_err(|e| MediaError::BadAnswer(format!("{}: {e}", path.display())))?;
        let mime = match bytes.first_chunk::<12>() {
            Some(b) if b.starts_with(b"\x89PNG\r\n\x1a\n") => "image/png",
            Some(b) if b.starts_with(b"\xff\xd8\xff") => "image/jpeg",
            Some(b) if b.starts_with(b"RIFF") && &b[8..12] == b"WEBP" => "image/webp",
            Some(b) if b.starts_with(b"GIF8") => "image/gif",
            _ => {
                return Err(MediaError::BadAnswer(format!(
                    "{} isn't a PNG, JPEG, WebP or GIF",
                    path.display()
                )))
            }
        };
        Ok(format!(
            "data:{mime};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        ))
    }
}

#[async_trait::async_trait]
impl MediaBackend for OpenRouterMedia {
    async fn generate_image(&self, req: ImageReq) -> Result<ImageOut, MediaError> {
        let mut body = json!({
            "model": req.model.as_deref().unwrap_or(&self.settings.image_model),
            "prompt": req.prompt,
        });
        if let Some(size) = &req.size {
            body["size"] = json!(size);
        }
        if !req.references.is_empty() {
            let mut refs = Vec::new();
            for r in &req.references {
                refs.push(
                    json!({ "type": "image_url", "image_url": { "url": Self::data_url(r)? } }),
                );
            }
            body["input_references"] = json!(refs);
        }
        let answer = self
            .checked(self.post(self.url("/images/generations"), &body))
            .await?;
        let first = answer["data"][0].clone();
        if first.is_null() {
            return Err(MediaError::BadAnswer("no image in the answer".into()));
        }
        if let Some(b64) = first["b64_json"].as_str() {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| MediaError::BadAnswer(format!("the image's base64: {e}")))?;
            return Ok(ImageOut {
                bytes,
                format: "png".into(),
            });
        }
        if let Some(url) = first["url"].as_str() {
            let dir = std::env::temp_dir();
            let tmp = dir.join(format!("ferrule-media-{}.bin", std::process::id()));
            self.download(url, &tmp).await?;
            let bytes = std::fs::read(&tmp)
                .map_err(|e| MediaError::BadAnswer(format!("the downloaded image: {e}")))?;
            let _ = std::fs::remove_file(&tmp);
            let format = sniff(&bytes).unwrap_or("png").to_string();
            return Ok(ImageOut { bytes, format });
        }
        Err(MediaError::BadAnswer(
            "the answer has neither b64_json nor a url".into(),
        ))
    }

    async fn submit_video(&self, req: VideoReq) -> Result<VideoJob, MediaError> {
        let mut body = json!({
            "model": req.model.as_deref().unwrap_or(&self.settings.video_model),
            "prompt": req.prompt,
        });
        if let Some(s) = req.seconds {
            body["duration"] = json!(s);
        }
        if let Some(a) = &req.aspect {
            body["aspect_ratio"] = json!(a);
        }
        if let Some(frame) = &req.first_frame {
            body["frame_images"] = json!([{
                "type": "image_url",
                "image_url": { "url": Self::data_url(frame)? },
                "frame_type": "first_frame",
            }]);
        }
        let answer = self.checked(self.post(self.url("/videos"), &body)).await?;
        let id = answer["id"]
            .as_str()
            .ok_or_else(|| MediaError::BadAnswer("the video answer has no id".into()))?;
        Ok(VideoJob { id: id.to_string() })
    }

    async fn poll_video(&self, job: &str) -> Result<VideoState, MediaError> {
        let mut req = self
            .client
            .get(self.url(&format!("/videos/{job}")))
            .timeout(self.settings.timeout);
        if let Some(key) = &self.settings.key {
            req = req.bearer_auth(key);
        }
        let answer = self.checked(req).await?;
        let status = answer["status"].as_str().unwrap_or_default();
        match status {
            "completed" | "succeeded" => {
                // OpenRouter's shape: outputs[0].url, with a content_url
                // fallback seen on some providers.
                let url = answer["outputs"][0]["url"]
                    .as_str()
                    .or_else(|| answer["content_url"].as_str())
                    .or_else(|| answer["video"]["url"].as_str())
                    .ok_or_else(|| {
                        MediaError::BadAnswer("the job is done but names no file".into())
                    })?;
                Ok(VideoState::Done {
                    url: url.to_string(),
                })
            }
            "failed" | "error" | "cancelled" => Ok(VideoState::Failed {
                reason: answer["error"]["message"]
                    .as_str()
                    .or_else(|| answer["error"].as_str())
                    .unwrap_or("the provider failed the job")
                    .chars()
                    .take(200)
                    .collect(),
            }),
            _ => Ok(VideoState::Pending),
        }
    }

    async fn download(&self, url: &str, dest: &Path) -> Result<(), MediaError> {
        let transport = |e: reqwest::Error| MediaError::Transport {
            host: self.host.clone(),
            message: e.without_url().to_string(),
        };
        let bytes = self
            .client
            .get(url)
            .timeout(self.settings.timeout.max(Duration::from_secs(120)))
            .send()
            .await
            .map_err(transport)?
            .error_for_status()
            .map_err(transport)?
            .bytes()
            .await
            .map_err(transport)?;
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| MediaError::BadAnswer(format!("{}: {e}", parent.display())))?;
        }
        std::fs::write(dest, &bytes)
            .map_err(|e| MediaError::BadAnswer(format!("{}: {e}", dest.display())))
    }
}

/// The format by the file's first bytes, never by its name.
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    match bytes.first_chunk::<12>() {
        Some(b) if b.starts_with(b"\x89PNG\r\n\x1a\n") => Some("png"),
        Some(b) if b.starts_with(b"\xff\xd8\xff") => Some("jpeg"),
        Some(b) if b.starts_with(b"RIFF") && &b[8..12] == b"WEBP" => Some("webp"),
        Some(b) if b.starts_with(b"GIF8") => Some("gif"),
        // ftyp box at byte 4: mp4/mov/isom…
        Some(b) if &b[4..8] == b"ftyp" => Some("mp4"),
        _ => None,
    }
}

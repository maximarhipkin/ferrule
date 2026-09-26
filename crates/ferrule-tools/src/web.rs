use ferrule_core::error::CoreError;
use ferrule_core::tool::{Tool, ToolContext, ToolDefinition, ToolOutput};
use ferrule_sandbox::Egress;
use serde_json::{json, Value};
use std::time::Duration;

/// Fetch a URL and return readable text (tags/scripts stripped, whitespace
/// collapsed, size-capped). Deliberately minimal: browser automation belongs
/// behind MCP, not in the core toolbelt.
pub struct WebFetchTool {
    pub timeout: Duration,
    /// The credential proxy, when there is one: HTTPS then goes through
    /// it, like sandboxed commands' requests do.
    pub egress: Option<Egress>,
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            egress: None,
        }
    }
}

impl WebFetchTool {
    pub fn with_egress(egress: Option<Egress>) -> Self {
        Self {
            egress,
            ..Self::default()
        }
    }
}

/// Naive but effective HTML-to-text: drop scripts/styles, drop tags, collapse
/// whitespace. Good enough for docs and articles; not a renderer.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut chars = html.chars().peekable();
    let mut skip_depth = 0i32;
    while let Some(c) = chars.next() {
        match c {
            '<' => {
                let mut tag = String::new();
                for tc in chars.by_ref() {
                    if tc == '>' {
                        break;
                    }
                    tag.push(tc);
                }
                let name = tag
                    .trim_start_matches('/')
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_lowercase();
                if name == "script" || name == "style" || name == "noscript" {
                    if tag.starts_with('/') {
                        skip_depth = (skip_depth - 1).max(0);
                    } else if !tag.ends_with('/') {
                        skip_depth += 1;
                    }
                }
                if skip_depth == 0
                    && matches!(
                        name.as_str(),
                        "p" | "div" | "br" | "li" | "h1" | "h2" | "h3" | "tr" | "section"
                    )
                {
                    out.push('\n');
                }
            }
            _ => {
                if skip_depth == 0 {
                    out.push(c);
                }
            }
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[async_trait::async_trait]
impl Tool for WebFetchTool {
    fn changes_files(&self) -> bool {
        false
    }
    fn read_only(&self) -> bool {
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "web_fetch".into(),
            description: "Fetch a web page and return its readable text content.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "url": { "type": "string", "description": "http(s) URL to fetch" } },
                "required": ["url"]
            }),
        }
    }

    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, CoreError> {
        let url = args["url"].as_str().unwrap_or("");
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(CoreError::ToolFailed {
                tool: "web_fetch".into(),
                message: "only http(s) URLs allowed".into(),
            });
        }
        let client = crate::egress::client_builder(self.egress.as_ref())
            .map_err(|e| CoreError::ToolFailed {
                tool: "web_fetch".into(),
                message: format!("the credential proxy's settings: {e}"),
            })?
            .timeout(self.timeout)
            .build()
            .map_err(|e| CoreError::Provider(e.to_string()))?;
        let resp = client
            .get(url)
            .header("User-Agent", "ferrule/0.1")
            .send()
            .await
            .map_err(|e| CoreError::ToolFailed {
                tool: "web_fetch".into(),
                message: e.to_string(),
            })?;
        if crate::egress::is_denial(&resp) {
            // Not the page: the policy's reason, as a failure the model
            // won't mistake for content (and shouldn't retry).
            let why = resp.text().await.unwrap_or_default();
            return Err(CoreError::ToolFailed {
                tool: "web_fetch".into(),
                message: why.trim().to_string(),
            });
        }
        let status = resp.status();
        let text = resp.text().await.map_err(|e| CoreError::ToolFailed {
            tool: "web_fetch".into(),
            message: e.to_string(),
        })?;
        let mut text = html_to_text(&text);
        // An error page is still worth reading, but not as if it were the
        // page asked for.
        if !status.is_success() {
            text = format!("HTTP {status}\n{text}");
        }
        Ok(ToolOutput::capped(text, ctx.max_output_chars))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_to_text_strips_scripts_and_tags() {
        let html = "<html><head><style>body{color:red}</style></head>\
                    <body><h1>Title</h1><p>Hello <b>world</b></p>\
                    <script>evil()</script></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Title"));
        assert!(text.contains("Hello world"));
        assert!(!text.contains("evil()"));
        assert!(!text.contains("color:red"));
    }
}

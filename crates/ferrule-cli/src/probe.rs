//! The live checks `ferrule setup` and `ferrule doctor` share: does this
//! key open the provider's model list, does this bot token work, who has
//! messaged the bot. Errors never carry a URL — a Telegram URL holds the
//! token.

use serde_json::Value;
use std::fmt;
use std::time::Duration;

/// Why a check didn't pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    /// The service said no to the credential (401/403, or Telegram's 404
    /// for a malformed token).
    Rejected(String),
    /// Telegram: someone else is reading this bot's updates, or a webhook
    /// is set.
    Conflict(String),
    /// Couldn't tell: network trouble, an unexpected status or body.
    Failed(String),
}

impl fmt::Display for Check {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Check::Rejected(why) | Check::Conflict(why) | Check::Failed(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for Check {}

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(45))
        .user_agent(concat!("ferrule/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("static client config")
}

fn transport(e: reqwest::Error) -> Check {
    Check::Failed(e.without_url().to_string())
}

/// `GET <base_url>/models` with the key: the model ids, sorted. Anthropic's
/// endpoint wants its own headers; everyone else takes a bearer token.
pub async fn models(
    http: &reqwest::Client,
    base_url: &str,
    key: &str,
    anthropic: bool,
) -> Result<Vec<String>, Check> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut req = http
        .get(url)
        .bearer_auth(key)
        .timeout(Duration::from_secs(15));
    if anthropic {
        req = req
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01");
    }
    let resp = req.send().await.map_err(transport)?;
    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(Check::Rejected(format!(
            "the key was rejected (HTTP {})",
            status.as_u16()
        )));
    }
    if !status.is_success() {
        return Err(Check::Failed(format!(
            "the model list returned HTTP {}",
            status.as_u16()
        )));
    }
    let body: Value = resp
        .json()
        .await
        .map_err(|_| Check::Failed("the model list isn't JSON".into()))?;
    model_ids(&body).ok_or_else(|| Check::Failed("unexpected model list shape".into()))
}

/// `GET url` with the token as a bearer: does the service take it? For
/// tool credentials (GitHub's `/user` and the like).
pub async fn token_accepted(http: &reqwest::Client, url: &str, token: &str) -> Result<(), Check> {
    let resp = http
        .get(url)
        .bearer_auth(token)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(transport)?;
    match resp.status().as_u16() {
        200..=299 => Ok(()),
        code @ (401 | 403) => Err(Check::Rejected(format!(
            "the token was rejected (HTTP {code})"
        ))),
        code => Err(Check::Failed(format!("HTTP {code}"))),
    }
}

/// `{"data": [{"id": …}, …]}` — OpenAI's shape, which Anthropic, Ollama,
/// OpenRouter, DeepSeek and Moonshot share.
pub fn model_ids(body: &Value) -> Option<Vec<String>> {
    let mut ids: Vec<String> = body
        .get("data")?
        .as_array()?
        .iter()
        .filter_map(|m| m.get("id")?.as_str().map(str::to_string))
        .collect();
    ids.sort();
    ids.dedup();
    Some(ids)
}

/// Does this look like a BotFather token (`<digits>:<secret>`)? Also keeps
/// anything that would break the URL out of it.
pub fn plausible_bot_token(token: &str) -> bool {
    token.split_once(':').is_some_and(|(id, secret)| {
        !id.is_empty()
            && id.bytes().all(|b| b.is_ascii_digit())
            && secret.len() >= 20
            && secret
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    })
}

pub struct Telegram<'a> {
    pub http: &'a reqwest::Client,
    pub base_url: &'a str,
    pub token: &'a str,
}

impl Telegram<'_> {
    async fn call(&self, method: &str, params: &Value) -> Result<Value, Check> {
        let url = format!(
            "{}/bot{}/{method}",
            self.base_url.trim_end_matches('/'),
            self.token
        );
        let resp = self
            .http
            .post(url)
            .json(params)
            .send()
            .await
            .map_err(transport)?;
        let status = resp.status().as_u16();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        if body.get("ok").and_then(Value::as_bool) == Some(true) {
            return Ok(body.get("result").cloned().unwrap_or(Value::Null));
        }
        let why = body
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("no description")
            .to_string();
        Err(match status {
            401 | 404 => Check::Rejected(format!("Telegram rejected the token ({why})")),
            409 => Check::Conflict(why),
            _ => Check::Failed(format!("Telegram {method}: HTTP {status}, {why}")),
        })
    }

    /// The bot's @username.
    pub async fn get_me(&self) -> Result<String, Check> {
        let me = self.call("getMe", &serde_json::json!({})).await?;
        me.get("username")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| Check::Failed("getMe returned no username".into()))
    }

    /// The webhook URL, if one is set — polling gets nothing while it is.
    pub async fn webhook(&self) -> Result<Option<String>, Check> {
        let info = self.call("getWebhookInfo", &serde_json::json!({})).await?;
        Ok(info
            .get("url")
            .and_then(Value::as_str)
            .filter(|u| !u.is_empty())
            .map(str::to_string))
    }

    pub async fn delete_webhook(&self) -> Result<(), Check> {
        self.call("deleteWebhook", &serde_json::json!({}))
            .await
            .map(drop)
    }

    /// Long-poll for messages. Passing `offset` also confirms every update
    /// below it, so the gateway won't see them again.
    pub async fn updates(
        &self,
        offset: Option<i64>,
        timeout_secs: u64,
    ) -> Result<Vec<Value>, Check> {
        let mut params = serde_json::json!({
            "timeout": timeout_secs,
            "allowed_updates": ["message"],
        });
        if let Some(offset) = offset {
            params["offset"] = offset.into();
        }
        let result = self.call("getUpdates", &params).await?;
        Ok(result.as_array().cloned().unwrap_or_default())
    }

    pub async fn send(&self, chat_id: i64, text: &str) -> Result<(), Check> {
        let params = serde_json::json!({ "chat_id": chat_id, "text": text });
        self.call("sendMessage", &params).await.map(drop)
    }
}

/// A chat that messaged the bot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeenChat {
    pub id: i64,
    /// "private", "group", "supergroup" or "channel".
    pub kind: String,
    /// A group's title, or a person's name and @username.
    pub name: String,
}

/// The chats in a batch of updates, first appearance order, and the offset
/// that confirms the whole batch.
pub fn seen_chats(updates: &[Value]) -> (Option<i64>, Vec<SeenChat>) {
    let mut next = None;
    let mut chats: Vec<SeenChat> = Vec::new();
    for update in updates {
        if let Some(id) = update.get("update_id").and_then(Value::as_i64) {
            next = next.max(Some(id + 1));
        }
        let Some(chat) = update.get("message").and_then(|m| m.get("chat")) else {
            continue;
        };
        let Some(id) = chat.get("id").and_then(Value::as_i64) else {
            continue;
        };
        if chats.iter().any(|c| c.id == id) {
            continue;
        }
        let field = |k: &str| chat.get(k).and_then(Value::as_str).unwrap_or("").trim();
        let person = [field("first_name"), field("last_name")]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" ");
        let (title, user) = (field("title"), field("username"));
        let name = if !title.is_empty() {
            title.to_string()
        } else {
            match (person.is_empty(), user.is_empty()) {
                (true, true) => format!("chat {id}"),
                (false, true) => person,
                (true, false) => format!("@{user}"),
                (false, false) => format!("{person} (@{user})"),
            }
        };
        let kind = field("type");
        chats.push(SeenChat {
            id,
            kind: if kind.is_empty() {
                "private".into()
            } else {
                kind.into()
            },
            name,
        });
    }
    (next, chats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn model_ids_are_sorted_and_deduplicated() {
        let body =
            json!({"object": "list", "data": [{"id": "b"}, {"id": "a"}, {"x": 1}, {"id": "b"}]});
        assert_eq!(model_ids(&body).unwrap(), ["a", "b"]);
        assert!(model_ids(&json!({"models": []})).is_none());
    }

    #[test]
    fn bot_tokens_are_checked_for_shape() {
        assert!(plausible_bot_token(
            "123456789:AAHfiqksKZ8WmR2zSjiQ7_v4TMAKdiHm9T0"
        ));
        assert!(!plausible_bot_token("123456789"));
        assert!(!plausible_bot_token(
            "abc:AAHfiqksKZ8WmR2zSjiQ7_v4TMAKdiHm9T0"
        ));
        assert!(!plausible_bot_token("123:short"));
        assert!(!plausible_bot_token(
            "123:AAHfiqksKZ8WmR2zSjiQ7/../getMe?x=1"
        ));
    }

    #[test]
    fn seen_chats_names_people_and_groups_once_each() {
        let updates = [
            json!({"update_id": 10, "message": {"chat": {"id": 42, "type": "private", "first_name": "Max", "username": "maxim"}}}),
            json!({"update_id": 11, "message": {"chat": {"id": -1001, "type": "supergroup", "title": "Team"}}}),
            json!({"update_id": 12, "message": {"chat": {"id": 42, "type": "private", "first_name": "Max"}}}),
            json!({"update_id": 13, "edited_message": {"chat": {"id": 7}}}),
            json!({"update_id": 14, "message": {"chat": {"id": 8, "type": "private", "username": "anon"}}}),
        ];
        let (next, chats) = seen_chats(&updates);
        assert_eq!(next, Some(15));
        let names: Vec<(i64, &str, &str)> = chats
            .iter()
            .map(|c| (c.id, c.kind.as_str(), c.name.as_str()))
            .collect();
        assert_eq!(
            names,
            [
                (42, "private", "Max (@maxim)"),
                (-1001, "supergroup", "Team"),
                (8, "private", "@anon")
            ]
        );
        assert_eq!(seen_chats(&[]), (None, vec![]));
    }
}

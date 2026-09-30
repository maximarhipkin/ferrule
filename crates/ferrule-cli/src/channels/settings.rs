//! M39: the new channels' config sub-tables, `[gateway.whatsapp]` and the
//! rest. A table that is present turns its channel on. Secrets are never
//! values here: each `*_env` names an env var (saved in `secrets.env`).

use serde::Deserialize;

fn default_max_file_mb() -> u64 {
    ferrule_gateway::channels::files::DEFAULT_MAX_MB
}

/// `[gateway.whatsapp]`: the WhatsApp Business Cloud API (M39 §3).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WhatsApp {
    /// The number's id from the app's API Setup page (not the number).
    pub phone_number_id: String,
    #[serde(default = "WhatsApp::default_token_env")]
    pub token_env: String,
    #[serde(default = "WhatsApp::default_app_secret_env")]
    pub app_secret_env: String,
    #[serde(default = "WhatsApp::default_verify_token_env")]
    pub verify_token_env: String,
    /// `relay` (the M20 Worker's mailbox) or `listen` (your own tunnel to
    /// `127.0.0.1:<listen_port>`).
    #[serde(default)]
    pub inbound: WhatsAppInbound,
    #[serde(default = "WhatsApp::default_listen_port")]
    pub listen_port: u16,
    /// The relay Worker's URL; unset: the instance's deployed relay.
    #[serde(default)]
    pub relay_url: Option<String>,
    #[serde(default = "WhatsApp::default_api_url")]
    pub api_url: String,
    #[serde(default = "WhatsApp::default_api_version")]
    pub api_version: String,
    /// A utility template with one `{{1}}`, sent when the 24-hour window
    /// is closed.
    #[serde(default)]
    pub template: Option<String>,
    #[serde(default = "WhatsApp::default_template_language")]
    pub template_language: String,
    /// `wa_id` digits (`972501234567`).
    #[serde(default)]
    pub allowed_users: Vec<String>,
    #[serde(default = "default_max_file_mb")]
    pub max_file_mb: u64,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WhatsAppInbound {
    #[default]
    Relay,
    Listen,
}

impl WhatsApp {
    fn default_token_env() -> String {
        "WHATSAPP_TOKEN".into()
    }
    fn default_app_secret_env() -> String {
        "WHATSAPP_APP_SECRET".into()
    }
    fn default_verify_token_env() -> String {
        "WHATSAPP_VERIFY_TOKEN".into()
    }
    fn default_listen_port() -> u16 {
        8787
    }
    fn default_api_url() -> String {
        "https://graph.facebook.com".into()
    }
    fn default_api_version() -> String {
        "v23.0".into()
    }
    fn default_template_language() -> String {
        "en_US".into()
    }
}

/// `[gateway.matrix]` (M39 §4).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Matrix {
    /// `https://matrix.example.org`.
    pub homeserver: String,
    /// The env var holding an access token (`syt_…`).
    #[serde(default)]
    pub access_token_env: Option<String>,
    /// Or log in with a password: the bot's `@user:server`… (beside a
    /// token, only whose token it is: setup writes it, and the instance
    /// clash check compares it)
    #[serde(default)]
    pub user: Option<String>,
    /// …and the env var holding its password.
    #[serde(default)]
    pub password_env: Option<String>,
    /// `@max:example.org`: whose DMs reach the agent.
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// `!room:server` ids where a mention reaches the agent.
    #[serde(default)]
    pub allowed_rooms: Vec<String>,
    #[serde(default)]
    pub stream: Option<bool>,
    #[serde(default = "default_max_file_mb")]
    pub max_file_mb: u64,
}

/// `[gateway.email]` (M39 §5).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Email {
    /// `gmail`: the M37 Gmail connection's address and app password.
    #[serde(default)]
    pub use_connection: Option<String>,
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub imap_host: Option<String>,
    #[serde(default)]
    pub imap_port: Option<u16>,
    #[serde(default)]
    pub smtp_host: Option<String>,
    #[serde(default)]
    pub smtp_port: Option<u16>,
    /// Defaults to `address`.
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password_env: Option<String>,
    /// Addresses and `@domain` entries whose mail reaches the agent.
    #[serde(default)]
    pub allowed_senders: Vec<String>,
    /// Demand `Authentication-Results` DMARC/SPF from the receiving server.
    /// Unset: on for Gmail.
    #[serde(default)]
    pub require_auth_results: Option<bool>,
    /// Without IDLE: how often to look for new mail.
    #[serde(default)]
    pub poll_secs: Option<u64>,
    #[serde(default = "default_max_file_mb")]
    pub max_file_mb: u64,
}

/// `[gateway.signal]` (M39 §6).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Signal {
    /// The registered or linked number, `+972…`.
    pub account: String,
    /// A daemon you run (`http://127.0.0.1:7583`); unset: ferrule spawns one.
    #[serde(default)]
    pub url: Option<String>,
    /// The `signal-cli` to spawn; unset: the one on PATH.
    #[serde(default)]
    pub signal_cli: Option<String>,
    /// The spawned daemon's port on 127.0.0.1.
    #[serde(default = "Signal::default_port")]
    pub port: u16,
    /// Numbers (`+972…`) or ACI uuids.
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// Group ids (base64) where a mention reaches the agent.
    #[serde(default)]
    pub allowed_groups: Vec<String>,
    #[serde(default = "default_max_file_mb")]
    pub max_file_mb: u64,
}

impl Signal {
    fn default_port() -> u16 {
        7583
    }
}

/// `[gateway.mattermost]` (M39 §7).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Mattermost {
    /// `https://chat.example.com`.
    pub server_url: String,
    #[serde(default = "Mattermost::default_token_env")]
    pub token_env: String,
    /// User ids (or usernames, resolved at start).
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// Channel ids where a mention reaches the agent.
    #[serde(default)]
    pub allowed_channels: Vec<String>,
    #[serde(default)]
    pub stream: Option<bool>,
    #[serde(default = "default_max_file_mb")]
    pub max_file_mb: u64,
}

impl Mattermost {
    fn default_token_env() -> String {
        "MATTERMOST_TOKEN".into()
    }
}

/// `[gateway.http]`: the HTTP API for programs (M39 §8).
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HttpApi {
    #[serde(default = "HttpApi::default_port")]
    pub port: u16,
    /// Where it listens (default 127.0.0.1; `FERRULE_HTTP_BIND` wins).
    #[serde(default)]
    pub bind: Option<String>,
    /// `tunnel`: M20's quick tunnel to it. Unset: 127.0.0.1 only.
    #[serde(default)]
    pub public: Option<String>,
    /// Requests a minute per client.
    #[serde(default = "HttpApi::default_rate")]
    pub requests_per_minute: u32,
    #[serde(default)]
    pub stream: Option<bool>,
}

impl HttpApi {
    fn default_port() -> u16 {
        8788
    }
    fn default_rate() -> u32 {
        30
    }
}

impl Default for HttpApi {
    fn default() -> Self {
        Self {
            port: Self::default_port(),
            bind: None,
            public: None,
            requests_per_minute: Self::default_rate(),
            stream: None,
        }
    }
}

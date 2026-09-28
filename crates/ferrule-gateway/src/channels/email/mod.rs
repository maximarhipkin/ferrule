//! Email adapter (M39 §5): IMAP in (IDLE when the server has it, else a
//! `NOOP` every `poll`), SMTP out, on M37's own IMAP, SMTP and MIME code.
//!
//! A chat is one sender address, so the session follows the person; a
//! reply threads under their last mail (`In-Reply-To`/`References`), and
//! anything ferrule starts (a notice, a task's result) opens a thread of
//! its own, "ferrule: <first line>".
//!
//! Nothing is ever answered that could start a loop or answer a list
//! ([`guard::loop_reason`]), strangers are dropped without a word, and an
//! approval by mail counts only when the receiving server vouched for the
//! `From:` (`Authentication-Results`).
//!
//! Only new mail is read: the first start remembers `UIDNEXT`, and
//! `state.json` keeps the last UID seen (per `UIDVALIDITY`), so nothing is
//! replayed after a restart or answered twice. Mail that isn't taken is
//! left unread.

pub mod guard;

use crate::channel::{Channel, ChannelCapabilities};
use crate::channels::files::{self, Inbox};
use crate::error::GatewayError;
use crate::message::{InboundMessage, OutboundMessage};
use ferrule_connections::native::imap::{self, Imap, Part};
use ferrule_connections::native::mime;
use ferrule_connections::native::net::{self, Addr};
use ferrule_connections::native::smtp::{self, Smtp};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

/// Mails to one address in an hour before sending stops (a loop no header
/// caught).
pub const BUDGET: usize = 10;
/// The mark in ferrule's own `Message-ID`s: `<hex.ferrule@domain>`.
const OWN_MARK: &str = ".ferrule@";
/// Room above `max_file_mb` for a mail's text and base64's growth.
const MAIL_SLACK: u64 = 2 * 1024 * 1024;

/// How a connection is secured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Security {
    /// TLS from the first byte (993, 465).
    Tls,
    /// A plain greeting, then STARTTLS (143, 587).
    StartTls,
    /// No TLS at all: only to this machine (a test's mock, a local bridge).
    Plain,
}

/// A mail server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    pub host: String,
    pub port: u16,
    pub security: Security,
}

impl Server {
    /// 993 and 465 are TLS; any other port is STARTTLS, except on this
    /// machine (127.0.0.1, ::1, localhost), where it is plain.
    pub fn new(host: &str, port: u16) -> Self {
        let security = if matches!(port, 993 | 465) {
            Security::Tls
        } else if is_loopback(host) {
            Security::Plain
        } else {
            Security::StartTls
        };
        Self {
            host: host.to_string(),
            port,
            security,
        }
    }

    fn addr(&self) -> Addr {
        if self.security == Security::Tls {
            Addr::tls(&self.host, self.port)
        } else {
            Addr::plain(&self.host, self.port)
        }
    }
}

pub fn is_loopback(host: &str) -> bool {
    let h = host.trim_matches(['[', ']']);
    h.eq_ignore_ascii_case("localhost")
        || h.parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Everything the adapter needs.
#[derive(Clone)]
pub struct EmailConfig {
    /// The mailbox's own address: `From:` of everything sent.
    pub address: String,
    pub username: String,
    pub password: String,
    pub imap: Server,
    pub smtp: Server,
    /// Mail whose `From:` the receiving server didn't vouch for is dropped.
    pub require_auth: bool,
    /// Without IDLE: how often to look.
    pub poll: Duration,
    /// `<data>/gateway/email`: the last UID seen and the threads.
    pub state_dir: Option<PathBuf>,
    /// Where files people send are saved; `None`: not saved.
    pub inbox: Option<Inbox>,
    /// `X-Ferrule-Loop`'s value: which instance sent a mail.
    pub instance: String,
}

impl std::fmt::Debug for EmailConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmailConfig")
            .field("address", &self.address)
            .field("username", &self.username)
            .field("password", &"…")
            .field("imap", &self.imap)
            .field("smtp", &self.smtp)
            .field("require_auth", &self.require_auth)
            .field("poll", &self.poll)
            .finish_non_exhaustive()
    }
}

/// Waits and retries, shortened in tests.
#[derive(Clone)]
struct Timing {
    /// The longest one IDLE runs before a fresh search: under health's
    /// 5 minutes, and far under the 29 RFC 2177 allows.
    idle: Duration,
    backoff_min: Duration,
    backoff_max: Duration,
    /// After a refused login: slow, so a wrong password doesn't get the
    /// account locked.
    refused: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            idle: Duration::from_secs(240),
            backoff_min: Duration::from_secs(2),
            backoff_max: Duration::from_secs(120),
            refused: Duration::from_secs(600),
        }
    }
}

/// The last mail from a person, which the next answer threads under.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct Thread {
    message_id: String,
    #[serde(default)]
    references: String,
    #[serde(default)]
    subject: String,
}

/// What `state.json` keeps across restarts.
#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    /// `username@imap_host`: whose UIDs these are.
    account: String,
    uidvalidity: u32,
    /// The last UID looked at; `None` before the first start.
    last_uid: Option<u32>,
    /// Address → the thread to answer in.
    threads: BTreeMap<String, Thread>,
}

/// Why a session with the IMAP server ended.
enum Stop {
    /// The gateway is shutting down.
    Closed,
    /// The login was refused.
    Refused(String),
    Broken(String),
}

impl From<imap::Error> for Stop {
    fn from(e: imap::Error) -> Self {
        Stop::Broken(e.text().to_string())
    }
}

pub struct EmailChannel {
    cfg: EmailConfig,
    allowed: Vec<String>,
    timing: Timing,
    state: Mutex<State>,
    /// This one runs the IMAP loop (the gateway) and writes `state.json`;
    /// `ferrule tasks run-now` only sends.
    running: AtomicBool,
    /// Address → when each of the last hour's mails went.
    sent: Mutex<HashMap<String, VecDeque<Instant>>>,
    last_poll: Mutex<Option<SystemTime>>,
    auth_problem: Mutex<Option<String>>,
    imap_problem: Mutex<Option<String>>,
    send_problem: Mutex<Option<String>>,
}

impl EmailChannel {
    pub fn new(cfg: EmailConfig) -> Self {
        let state = cfg
            .state_dir
            .as_deref()
            .and_then(read_state)
            .unwrap_or_default();
        Self {
            cfg,
            allowed: vec![],
            timing: Timing::default(),
            state: Mutex::new(state),
            running: AtomicBool::new(false),
            sent: Mutex::new(HashMap::new()),
            last_poll: Mutex::new(None),
            auth_problem: Mutex::new(None),
            imap_problem: Mutex::new(None),
            send_problem: Mutex::new(None),
        }
    }

    /// Addresses (`max@example.com`) and domains (`@example.com`) whose
    /// mail reaches the agent.
    pub fn with_allowed(mut self, senders: Vec<String>) -> Self {
        self.allowed = senders
            .into_iter()
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        self
    }

    /// Tests: waits in milliseconds, not minutes.
    #[doc(hidden)]
    pub fn with_fast_retries(mut self) -> Self {
        self.timing = Timing {
            idle: Duration::from_millis(300),
            backoff_min: Duration::from_millis(20),
            backoff_max: Duration::from_millis(100),
            refused: Duration::from_millis(200),
        };
        self
    }

    fn account(&self) -> String {
        format!(
            "{}@{}",
            self.cfg.username.to_ascii_lowercase(),
            self.cfg.imap.host.to_ascii_lowercase()
        )
    }

    fn refused_words(&self, server: &str, why: &str) -> String {
        let why = why.trim();
        format!(
            "the {server} server refused the login for {}{}: most providers (Gmail, Yahoo, iCloud, Outlook) want an app password here, not the account's own password",
            self.cfg.username,
            if why.is_empty() {
                String::new()
            } else {
                format!(" ({why})")
            }
        )
    }

    /// Connects, secures and logs in: the session and whether it has IDLE.
    async fn open(&self) -> Result<(Imap, bool), Stop> {
        let server = &self.cfg.imap;
        let stream = net::connect(&server.addr()).await.map_err(Stop::Broken)?;
        let mut imap = Imap::start(stream).await?;
        if server.security == Security::StartTls {
            imap = imap.starttls(&server.host).await?;
        }
        match imap.login(&self.cfg.username, &self.cfg.password).await {
            Ok(()) => {}
            Err(imap::Error::No(why)) => {
                return Err(Stop::Refused(self.refused_words("IMAP", &why)))
            }
            Err(e) => return Err(e.into()),
        }
        let caps = imap.capabilities().await.unwrap_or_default();
        Ok((imap, caps.iter().any(|c| c == "IDLE")))
    }

    /// The IMAP loop: reconnects with a backoff until the gateway stops.
    async fn run_imap(&self, tx: tokio::sync::mpsc::Sender<InboundMessage>) {
        self.running.store(true, Ordering::Relaxed);
        let mut backoff = self.timing.backoff_min;
        loop {
            let wait = match self.session(&tx, &mut backoff).await {
                Stop::Closed => return,
                Stop::Refused(why) => {
                    tracing::warn!("email: {why}");
                    *self.auth_problem.lock().unwrap() = Some(why);
                    self.timing.refused
                }
                Stop::Broken(why) => {
                    tracing::warn!("email: {why}; reconnecting");
                    *self.imap_problem.lock().unwrap() =
                        Some(format!("email: {why} (reconnecting)"));
                    let wait = backoff;
                    backoff = (backoff * 2).min(self.timing.backoff_max);
                    wait
                }
            };
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = tx.closed() => return,
            }
        }
    }

    async fn session(
        &self,
        tx: &tokio::sync::mpsc::Sender<InboundMessage>,
        backoff: &mut Duration,
    ) -> Stop {
        let (mut imap, idle) = match self.open().await {
            Ok(x) => x,
            Err(stop) => return stop,
        };
        *self.auth_problem.lock().unwrap() = None;
        let stop = self.watch(&mut imap, idle, tx, backoff).await;
        if matches!(stop, Stop::Closed) {
            imap.logout().await;
        }
        stop
    }

    async fn watch(
        &self,
        imap: &mut Imap,
        idle: bool,
        tx: &tokio::sync::mpsc::Sender<InboundMessage>,
        backoff: &mut Duration,
    ) -> Stop {
        let (validity, next) = match imap.select("INBOX").await {
            Ok(x) => x,
            Err(e) => return e.into(),
        };
        if let Err(e) = self.reconcile(imap, validity, next).await {
            return e.into();
        }
        loop {
            let last = self.state.lock().unwrap().last_uid.unwrap_or(0);
            let uids = match imap.uids_after(last).await {
                Ok(u) => u,
                Err(e) => return e.into(),
            };
            for uid in uids {
                match self.take(imap, uid, tx).await {
                    Ok(()) => {}
                    Err(stop) => return stop,
                }
                self.passed(uid);
            }
            self.healthy(backoff);
            let waited = if idle {
                tokio::select! {
                    r = imap.idle(self.timing.idle) => r.map(|_| ()),
                    _ = tx.closed() => return Stop::Closed,
                }
            } else {
                tokio::select! {
                    _ = tokio::time::sleep(self.cfg.poll) => {}
                    _ = tx.closed() => return Stop::Closed,
                }
                imap.noop().await
            };
            if let Err(e) = waited {
                return e.into();
            }
            self.healthy(backoff);
        }
    }

    fn passed(&self, uid: u32) {
        let mut s = self.state.lock().unwrap();
        if s.last_uid.is_some_and(|l| l >= uid) {
            return;
        }
        s.last_uid = Some(uid);
        drop(s);
        self.save_state();
    }

    fn healthy(&self, backoff: &mut Duration) {
        *self.last_poll.lock().unwrap() = Some(SystemTime::now());
        *self.imap_problem.lock().unwrap() = None;
        *backoff = self.timing.backoff_min;
    }

    /// A new account, or a mailbox whose UIDs were renumbered
    /// (`UIDVALIDITY` changed): start from what's there now, never replay.
    async fn reconcile(
        &self,
        imap: &mut Imap,
        validity: u32,
        next: u32,
    ) -> Result<(), imap::Error> {
        let account = self.account();
        let fresh = {
            let s = self.state.lock().unwrap();
            s.account != account || s.uidvalidity != validity || s.last_uid.is_none()
        };
        if !fresh {
            return Ok(());
        }
        let last = if next > 0 {
            next - 1
        } else {
            imap.uids_after(0).await?.last().copied().unwrap_or(0)
        };
        {
            let mut s = self.state.lock().unwrap();
            if s.account != account {
                s.threads.clear();
            }
            s.account = account;
            s.uidvalidity = validity;
            s.last_uid = Some(last);
        }
        tracing::info!("email: watching INBOX from UID {}", last + 1);
        self.save_state();
        Ok(())
    }

    /// One new mail: judged by its headers, then read, handed on and
    /// marked read, or left alone.
    async fn take(
        &self,
        imap: &mut Imap,
        uid: u32,
        tx: &tokio::sync::mpsc::Sender<InboundMessage>,
    ) -> Result<(), Stop> {
        let id = uid.to_string();
        let lines = imap
            .command(&[
                Part::Atom("UID FETCH"),
                Part::Atom(&id),
                Part::Atom("(UID RFC822.SIZE BODY.PEEK[HEADER])"),
            ])
            .await?;
        let Some(line) = lines.into_iter().find(|l| !l.literals.is_empty()) else {
            return Ok(());
        };
        let size: u64 = line
            .text
            .split("RFC822.SIZE ")
            .nth(1)
            .and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next())
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        let raw_head = line.literals.into_iter().next().unwrap_or_default();
        let (h, _) = mime::headers(&raw_head);
        if let Some(why) = guard::loop_reason(&h, OWN_MARK) {
            tracing::debug!(uid, "email: not answered: {why}");
            return Ok(());
        }
        let from_header = h
            .iter()
            .find(|(n, _)| n == "from")
            .map(|(_, v)| mime::decode_words(v))
            .unwrap_or_default();
        let from = guard::bare(&from_header);
        if !from.contains('@') || !guard::allowed(&self.allowed, &from) {
            tracing::debug!(
                uid,
                "email: a sender not on [gateway.email] allowed_senders"
            );
            return Ok(());
        }
        let domain = from.rsplit_once('@').map(|(_, d)| d).unwrap_or("");
        let vouched = guard::authenticated(&h, domain);
        if self.cfg.require_auth && !vouched {
            tracing::warn!(
                uid,
                "email: mail from {from} dropped: the receiving server didn't vouch for its From: (Authentication-Results; require_auth_results)"
            );
            return Ok(());
        }
        let header = |name: &str| {
            h.iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        let thread = Thread {
            message_id: header("message-id"),
            references: header("references"),
            subject: mime::decode_words(&header("subject")),
        };
        self.remember_thread(&from, thread.clone());
        let cap = self
            .cfg
            .inbox
            .as_ref()
            .map_or(files::DEFAULT_MAX_MB * 1024 * 1024, Inbox::max_bytes)
            + MAIL_SLACK;
        if size > cap {
            let text = format!(
                "Your mail is {}, more than I can take ({}). Please send it smaller, or share a link to the files instead.",
                files::human(size),
                files::human(cap)
            );
            self.reply_quietly(&from, &text).await;
            imap.mark_seen(uid).await?;
            return Ok(());
        }
        let Some((_, raw)) = imap.fetch(&[uid], "BODY.PEEK[]").await?.into_iter().next() else {
            return Ok(());
        };
        let msg = mime::parse(&raw);
        let mut text = guard::cut_quoted(&msg.text);
        if let Some(answer) = guard::approval_line(&text) {
            if !vouched {
                tracing::warn!("email: an approval from {from} refused: not vouched for");
                self.reply_quietly(
                    &from,
                    "I can't take an approval by email from this message: your mail server's check of the sender (DMARC, SPF or DKIM in Authentication-Results) didn't pass, so it could have been sent by anyone. Please answer in another channel, or from a mailbox whose provider signs its mail.",
                )
                .await;
                imap.mark_seen(uid).await?;
                return Ok(());
            }
            text = answer;
        } else if !thread.subject.trim().is_empty()
            && !thread
                .subject
                .trim()
                .to_ascii_lowercase()
                .starts_with("re:")
        {
            text = format!("Subject: {}\n\n{text}", thread.subject.trim());
        }
        let message_id = if thread.message_id.is_empty() {
            format!("uid:{uid}")
        } else {
            thread.message_id.clone()
        };
        let (saved, refused) = self.save_files(&raw, &message_id);
        let text = files::with_notes(&text, &saved, &refused);
        if text.trim().is_empty() {
            imap.mark_seen(uid).await?;
            return Ok(());
        }
        let name = guard::display_name(&from_header);
        let inbound = InboundMessage {
            channel: "email".into(),
            chat_id: from.clone(),
            sender: name,
            sender_id: Some(from),
            message_id,
            text,
            attachments: saved
                .iter()
                .map(|s| crate::message::Attachment {
                    kind: s.mime.clone(),
                    url: s.path.to_string_lossy().into_owned(),
                    name: Some(s.rel.clone()),
                })
                .collect(),
            reply_to: None,
            ts: chrono::Utc::now().timestamp(),
        };
        // Past it before it's handed on: a restart right after never
        // answers it twice.
        self.passed(uid);
        if tx.send(inbound).await.is_err() {
            return Err(Stop::Closed);
        }
        imap.mark_seen(uid).await?;
        Ok(())
    }

    fn save_files(&self, raw: &[u8], id: &str) -> (Vec<files::Saved>, Vec<files::Refused>) {
        let mut saved = vec![];
        let mut refused = vec![];
        for f in mime::files(raw) {
            let Some(inbox) = &self.cfg.inbox else {
                refused.push(files::Refused {
                    name: f.name,
                    why: "files aren't saved on this gateway".into(),
                });
                continue;
            };
            if f.bytes.len() as u64 > inbox.max_bytes() {
                refused.push(inbox.too_big(&f.name, f.bytes.len() as u64));
                continue;
            }
            let mime = (!f.content_type.is_empty()).then_some(f.content_type.as_str());
            match inbox.save(
                "email",
                id.trim_matches(['<', '>']),
                &f.name,
                mime,
                &f.bytes,
            ) {
                Ok(s) => saved.push(s),
                Err(e) => refused.push(files::Refused {
                    name: f.name,
                    why: format!("it couldn't be saved ({e})"),
                }),
            }
        }
        (saved, refused)
    }

    fn remember_thread(&self, from: &str, thread: Thread) {
        if thread.message_id.is_empty() {
            return;
        }
        self.state
            .lock()
            .unwrap()
            .threads
            .insert(from.to_string(), thread);
    }

    /// A note to a sender from inside the loop: failures are logged only.
    async fn reply_quietly(&self, to: &str, text: &str) {
        if let Err(e) = self.deliver(to, text, &[]).await {
            tracing::warn!("email: couldn't tell {to}: {e}");
        }
    }

    fn save_state(&self) {
        let Some(dir) = &self.cfg.state_dir else {
            return;
        };
        if !self.running.load(Ordering::Relaxed) {
            return;
        }
        let state = self.state.lock().unwrap();
        write_json(dir, "state.json", &*state);
    }

    /// The thread to answer `to` in: the gateway's own memory, or, in a
    /// `ferrule tasks run-now` beside it, what the gateway last saved.
    fn thread_for(&self, to: &str) -> Option<Thread> {
        if !self.running.load(Ordering::Relaxed) {
            if let Some(disk) = self.cfg.state_dir.as_deref().and_then(read_state) {
                if disk.account == self.account() {
                    *self.state.lock().unwrap() = disk;
                }
            }
        }
        self.state.lock().unwrap().threads.get(to).cloned()
    }

    /// Takes one of `to`'s hourly budget, or says why not.
    fn spend(&self, to: &str) -> Result<(), GatewayError> {
        let mut sent = self.sent.lock().unwrap();
        let q = sent.entry(to.to_string()).or_default();
        let hour = Duration::from_secs(3600);
        while q.front().is_some_and(|t| t.elapsed() >= hour) {
            q.pop_front();
        }
        if q.len() >= BUDGET {
            let why = format!(
                "email: {BUDGET} mails to {to} in the last hour; holding the rest (a reply loop?)"
            );
            tracing::warn!("{why}");
            *self.send_problem.lock().unwrap() = Some(why.clone());
            return Err(GatewayError::Channel(why));
        }
        q.push_back(Instant::now());
        Ok(())
    }

    async fn deliver(
        &self,
        to: &str,
        text: &str,
        attached: &[mime::File],
    ) -> Result<(), GatewayError> {
        let to = to.trim().to_ascii_lowercase();
        if !valid_address(&to) {
            return Err(GatewayError::Channel(format!(
                "email: {to:?} isn't an address to send to"
            )));
        }
        self.spend(&to)?;
        let thread = self.thread_for(&to);
        let subject = match &thread {
            Some(t) if !t.subject.trim().is_empty() => guard::re(&t.subject),
            Some(_) => "Re: your mail".to_string(),
            None => format!("ferrule: {}", first_line(text, 60)),
        };
        let domain = self
            .cfg
            .address
            .rsplit_once('@')
            .map_or("ferrule.local", |(_, d)| d)
            .trim_end_matches('>')
            .to_string();
        let r: [u8; 12] = ferrule_connections::seal::random();
        let hex: String = r.iter().map(|b| format!("{b:02x}")).collect();
        let headers = [
            (
                "Auto-Submitted",
                if thread.is_some() {
                    "auto-replied".to_string()
                } else {
                    "auto-generated".to_string()
                },
            ),
            ("X-Ferrule-Loop", self.cfg.instance.clone()),
        ];
        let recipients = [to.clone()];
        let raw = mime::build(&mime::Outgoing {
            from: &self.cfg.address,
            to: &recipients,
            cc: &[],
            subject: &subject,
            body: text,
            in_reply_to: thread.as_ref().map(|t| t.message_id.as_str()),
            references: thread.as_ref().map(|t| t.references.as_str()),
            message_id: Some(format!("<{hex}{OWN_MARK}{domain}>")),
            date: mime::date_now(),
            headers: &headers,
            files: attached,
        });
        match smtp_send(&self.cfg, &recipients, &raw).await {
            Ok(()) => {
                *self.send_problem.lock().unwrap() = None;
                Ok(())
            }
            Err(e) => {
                let why = if e.auth {
                    self.refused_words("SMTP", &e.text)
                } else {
                    format!("email: sending to {to} failed: {}", e.text)
                };
                *self.send_problem.lock().unwrap() = Some(why.clone());
                Err(GatewayError::Channel(why))
            }
        }
    }
}

async fn smtp_send(cfg: &EmailConfig, to: &[String], raw: &str) -> Result<(), smtp::Error> {
    let mut s = smtp_login(cfg).await?;
    let sent = s.send_mail(&mime::bare(&cfg.address), to, raw).await;
    s.quit().await;
    sent
}

async fn smtp_login(cfg: &EmailConfig) -> Result<Smtp, smtp::Error> {
    let server = &cfg.smtp;
    let stream = net::connect(&server.addr())
        .await
        .map_err(|text| smtp::Error { text, auth: false })?;
    let mut s = Smtp::start(stream).await?;
    if server.security == Security::StartTls {
        s = s.starttls(&server.host).await?;
    }
    s.login(&cfg.username, &cfg.password)
        .await
        .map_err(|e| smtp::Error { auth: true, ..e })?;
    Ok(s)
}

fn valid_address(a: &str) -> bool {
    let Some((local, domain)) = a.rsplit_once('@') else {
        return false;
    };
    !local.is_empty()
        && domain.contains('.')
        && !a
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "<>,;\"".contains(c))
}

fn first_line(text: &str, max: usize) -> String {
    let line = text
        .lines()
        .map(|l| l.trim().trim_start_matches('#').trim())
        .find(|l| !l.is_empty())
        .unwrap_or("a message");
    if line.chars().count() <= max {
        line.to_string()
    } else {
        let cut: String = line.chars().take(max - 1).collect();
        format!("{}…", cut.trim_end())
    }
}

fn read_state(dir: &Path) -> Option<State> {
    let s = std::fs::read_to_string(dir.join("state.json")).ok()?;
    serde_json::from_str(&s).ok()
}

/// Writes `value` to `<dir>/<name>` through a temporary file.
fn write_json<T: Serialize>(dir: &Path, name: &str, value: &T) {
    let Ok(body) = serde_json::to_string_pretty(value) else {
        return;
    };
    let tmp = dir.join(format!(".{name}.tmp"));
    let r = std::fs::create_dir_all(dir)
        .and_then(|_| std::fs::write(&tmp, body))
        .and_then(|_| std::fs::rename(&tmp, dir.join(name)));
    if let Err(e) = r {
        tracing::warn!(error = %e, "email: couldn't save {name}");
    }
}

/// What a probe found.
#[derive(Debug, Clone)]
pub struct Probe {
    pub address: String,
    pub idle: bool,
    pub poll: Duration,
}

impl Probe {
    /// "max@example.com · IDLE" or "… · polling every 60 s".
    pub fn summary(&self) -> String {
        if self.idle {
            format!("{} · IDLE", self.address)
        } else {
            format!(
                "{} · no IDLE, polling every {} s",
                self.address,
                self.poll.as_secs().max(1)
            )
        }
    }
}

/// Logs in to both servers and opens INBOX read-only: nothing is sent or
/// marked read.
pub async fn probe(cfg: EmailConfig) -> Result<Probe, String> {
    let ch = EmailChannel::new(cfg.clone());
    let (mut imap, idle) = match ch.open().await {
        Ok(x) => x,
        Err(Stop::Refused(why)) => return Err(why),
        Err(Stop::Broken(why)) => {
            return Err(format!("IMAP {}:{}: {why}", cfg.imap.host, cfg.imap.port))
        }
        Err(Stop::Closed) => return Err("stopped".into()),
    };
    let opened = imap.examine("INBOX").await;
    imap.logout().await;
    opened.map_err(|e| format!("IMAP: couldn't open INBOX: {}", e.text()))?;
    match smtp_login(&cfg).await {
        Ok(s) => s.quit().await,
        Err(e) if e.auth => return Err(ch.refused_words("SMTP", &e.text)),
        Err(e) => {
            return Err(format!(
                "SMTP {}:{}: {}",
                cfg.smtp.host, cfg.smtp.port, e.text
            ))
        }
    }
    Ok(Probe {
        address: cfg.address,
        idle,
        poll: cfg.poll,
    })
}

#[async_trait::async_trait]
impl Channel for EmailChannel {
    fn name(&self) -> &str {
        "email"
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            attachments: true,
            ..Default::default()
        }
    }

    fn polls(&self) -> bool {
        true
    }

    fn last_ok_poll(&self) -> Option<SystemTime> {
        *self.last_poll.lock().unwrap()
    }

    fn problem(&self) -> Option<String> {
        self.auth_problem
            .lock()
            .unwrap()
            .clone()
            .or_else(|| self.imap_problem.lock().unwrap().clone())
            .or_else(|| self.send_problem.lock().unwrap().clone())
    }

    async fn run(&self, tx: tokio::sync::mpsc::Sender<InboundMessage>) -> Result<(), GatewayError> {
        self.run_imap(tx).await;
        Ok(())
    }

    async fn send(&self, msg: OutboundMessage) -> Result<(), GatewayError> {
        let mut attached = vec![];
        for att in &msg.attachments {
            let (name, content_type, bytes) = files::read_outgoing(att)?;
            attached.push(mime::File {
                name,
                content_type: content_type.to_string(),
                bytes,
            });
        }
        let text = if msg.text.trim().is_empty() && !attached.is_empty() {
            "(attached)".to_string()
        } else {
            msg.text.clone()
        };
        self.deliver(&msg.chat_id, &text, &attached).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_and_hosts_choose_the_security() {
        assert_eq!(Server::new("imap.gmail.com", 993).security, Security::Tls);
        assert_eq!(Server::new("smtp.gmail.com", 465).security, Security::Tls);
        assert_eq!(
            Server::new("smtp.mail.me.com", 587).security,
            Security::StartTls
        );
        assert_eq!(
            Server::new("mail.example.com", 143).security,
            Security::StartTls
        );
        assert_eq!(Server::new("127.0.0.1", 1143).security, Security::Plain);
        assert_eq!(Server::new("localhost", 1025).security, Security::Plain);
        assert_eq!(Server::new("[::1]", 1025).security, Security::Plain);
        assert_eq!(Server::new("127.0.0.1", 993).security, Security::Tls);
    }

    #[test]
    fn addresses_and_subjects() {
        assert!(valid_address("max@example.com"));
        assert!(!valid_address("max"));
        assert!(!valid_address("a@b.com>\r\nBcc: x@y.z"));
        assert!(!valid_address("a@localhost"));
        assert_eq!(
            first_line("\n# Done: the report\nmore", 60),
            "Done: the report"
        );
        assert_eq!(
            first_line(&"x".repeat(80), 10),
            format!("{}…", "x".repeat(9))
        );
    }

    #[test]
    fn the_config_hides_the_password() {
        let cfg = EmailConfig {
            address: "a@b.c".into(),
            username: "a@b.c".into(),
            password: "hunter2hunter2".into(),
            imap: Server::new("imap.b.c", 993),
            smtp: Server::new("smtp.b.c", 465),
            require_auth: true,
            poll: Duration::from_secs(60),
            state_dir: None,
            inbox: None,
            instance: "default".into(),
        };
        assert!(!format!("{cfg:?}").contains("hunter2"));
    }
}

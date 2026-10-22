//! M65 §5: the Files page (`docs/m65-video.md` §5). Big files go up in
//! pieces, and the owner sees, plays, downloads and deletes what was
//! uploaded, what chats sent and what the agent made.
//!
//! **Uploads.** `POST /api/files/upload/start {name, size, for?}` makes
//! `uploads/.part/<id>` and a note of what it will be. `PUT
//! /api/files/upload/<id>?offset=<n>` appends one piece of at most
//! [`CHUNK`], with the checks every POST gets (the session, the origin,
//! the CSRF header) and one piece in memory at a time (the HTTP server's
//! big-body slots). A piece at the wrong offset is answered 409 with the
//! offset the server has, so a dropped connection resumes there; `GET
//! /api/files/upload?id=` says how far one got. `POST
//! /api/files/upload/finish {id}` checks the size and moves the file to
//! `uploads/<day>/<name>`. A part nobody finished goes after a day.
//!
//! **The library.** `GET /api/files` lists `uploads/`, `inbox/` and
//! `media/`, newest first, with what `media_jobs` knows of what the agent
//! made. `GET /api/files/raw?path=` serves one: only inside those folders,
//! no symlinks, a `Range` answered, sent in pieces and never read whole;
//! only images, video and audio inline, anything else as a download, so an
//! uploaded page never runs on the dashboard's origin. `GET
//! /api/files/thumb?path=` is a poster: an image itself, or a video's
//! frame made once with ffmpeg in the sandbox under `.ferrule/thumbs/`.
//! `POST /api/files/delete {path}` moves one to `.ferrule/trash/`, which
//! housekeeping empties after 7 days. `POST /api/files/say {path, text}`
//! hands one to the page's chat with the owner's words.
//!
//! **From a chat.** A file too big for the owner's own chat (Telegram's
//! 20 MB for bots) gets them a one-time login link to this page
//! ([`TooBigDoor`]); the upload it opens wakes that chat's session with
//! where the file is.

use super::api::{bad, ok, Answer};
use super::http::{Request, Response};
use super::Ctx;
use ferrule_gateway::channels::files::{self, human, mime_for, safe_name};
use ferrule_gateway::tools::Ffmpeg;
use ferrule_media::{JobStore, MediaJob};
use ferrule_sandbox::Sandbox;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// The biggest piece one `PUT` carries: well under a tunnel's 100 MB on
/// one request.
pub const CHUNK: usize = 32 * 1024 * 1024;
/// Where pieces gather, under the workspace.
const PARTS: &str = "uploads/.part";
/// An upload is refused when it would leave less than this free.
const FREE_FLOOR: u64 = 1024 * 1024 * 1024;
/// A part nobody finished, and a chat's upload ticket, last this long.
const KEEP: Duration = Duration::from_secs(24 * 3600);
/// Video posters, under the workspace.
pub const THUMBS: &str = ".ferrule/thumbs";
/// What the page deleted, under the workspace.
pub const TRASH: &str = ".ferrule/trash";
/// The folders the library shows.
const ROOTS: [&str; 3] = ["uploads", "inbox", "media"];
/// The most items one listing returns.
const LIST_MAX: usize = 500;
/// How deep the listing looks under a root.
const DEPTH: usize = 5;
/// The longest the owner's words with a file may be.
const MAX_TEXT: usize = super::chat::MAX_TEXT;

/// The gateway's library: one per process, in the page's [`Ctx`].
pub struct Library {
    workspace: PathBuf,
    max_upload: u64,
    jobs: Option<Arc<JobStore>>,
    /// The sandbox and the programs posters are made with.
    video: Option<(Arc<Sandbox>, Ffmpeg)>,
    /// Upload tickets for the chats a too-big file came from: ticket →
    /// (session, when).
    tickets: Mutex<HashMap<String, (String, Instant)>>,
    /// Pieces are appended, and uploads finished, one at a time.
    writing: tokio::sync::Mutex<()>,
    /// Posters are made two at a time.
    posters: tokio::sync::Semaphore,
}

/// What an unfinished upload will be (`uploads/.part/<id>.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Pending {
    name: String,
    size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ticket: Option<String>,
}

impl Library {
    pub fn new(
        workspace: PathBuf,
        max_upload_mb: u64,
        jobs: Option<Arc<JobStore>>,
        video: Option<(Arc<Sandbox>, Ffmpeg)>,
    ) -> Self {
        Self {
            workspace: dunce::canonicalize(&workspace).unwrap_or(workspace),
            max_upload: max_upload_mb.max(1) * 1024 * 1024,
            jobs,
            video,
            tickets: Mutex::default(),
            writing: tokio::sync::Mutex::new(()),
            posters: tokio::sync::Semaphore::new(2),
        }
    }

    /// A ticket that ties an upload to `session` (a chat), for a day.
    pub fn ticket(&self, session: &str) -> String {
        let t = uuid::Uuid::new_v4().simple().to_string();
        let mut all = self.tickets.lock().unwrap();
        all.retain(|_, (_, at)| at.elapsed() < KEEP);
        all.insert(t.clone(), (session.to_string(), Instant::now()));
        t
    }

    /// The chat a ticket was made for, used up.
    fn take_ticket(&self, ticket: &str) -> Option<String> {
        let (session, at) = self.tickets.lock().unwrap().remove(ticket)?;
        (at.elapsed() < KEEP).then_some(session)
    }

    fn known_ticket(&self, ticket: &str) -> bool {
        self.tickets.lock().unwrap().contains_key(ticket)
    }

    /// `uploads/.part`, made if missing, refused when it leads outside the
    /// workspace; parts older than a day go.
    fn parts(&self) -> Result<PathBuf, String> {
        let dir = self.workspace.join(PARTS);
        std::fs::create_dir_all(&dir).map_err(|e| format!("{PARTS} can't be made: {e}"))?;
        let real = dunce::canonicalize(&dir).map_err(|e| format!("{PARTS}: {e}"))?;
        if !real.starts_with(&self.workspace) {
            return Err(format!("{PARTS} leads outside the workspace"));
        }
        if let Ok(entries) = std::fs::read_dir(&real) {
            for e in entries.flatten() {
                let old = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age > KEEP);
                if old {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        Ok(real)
    }

    /// An unfinished upload's part file and what it will be.
    fn pending(&self, id: &str) -> Result<(PathBuf, Pending), String> {
        let gone =
            || "That upload is gone (a day passed, or it finished): start it again.".to_string();
        if !valid_id(id) {
            return Err(gone());
        }
        let dir = self.parts()?;
        let note = std::fs::read(dir.join(format!("{id}.json"))).map_err(|_| gone())?;
        let pending: Pending = serde_json::from_slice(&note).map_err(|_| gone())?;
        let part = dir.join(id);
        if !part.is_file() {
            return Err(gone());
        }
        Ok((part, pending))
    }

    /// `rel`, as the page names a file, inside one of [`ROOTS`]: no `..`,
    /// no symlink anywhere along it, not a part file, and really under the
    /// workspace.
    fn inside(&self, rel: &str) -> Result<PathBuf, String> {
        let missing = || "There's no such file in the library.".to_string();
        let rel = Path::new(rel.trim());
        let mut parts = rel.components();
        let first = match parts.next() {
            Some(Component::Normal(f)) => f.to_string_lossy().into_owned(),
            _ => return Err(missing()),
        };
        if !ROOTS.contains(&first.as_str())
            || rel.components().any(|c| !matches!(c, Component::Normal(_)))
            || rel.starts_with(PARTS)
        {
            return Err(missing());
        }
        let mut at = self.workspace.clone();
        for c in rel.components() {
            at.push(c);
            let meta = std::fs::symlink_metadata(&at).map_err(|_| missing())?;
            if meta.file_type().is_symlink() {
                return Err(missing());
            }
        }
        let real = dunce::canonicalize(&at).map_err(|_| missing())?;
        if !real.starts_with(self.workspace.join(&first)) || !real.is_file() {
            return Err(missing());
        }
        Ok(real)
    }

    /// A video's poster, made once (from its first second, else its first
    /// frame) and kept by the video's path and modification time.
    async fn poster(&self, video: &Path) -> Option<PathBuf> {
        let (sandbox, ffmpeg) = self.video.as_ref()?;
        let modified = std::fs::metadata(video).ok()?.modified().ok()?;
        let nanos = modified
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let key = ferrule_core::fnv64(format!("{}@{nanos}", video.display()).as_bytes());
        let dir = self.workspace.join(THUMBS);
        std::fs::create_dir_all(&dir).ok()?;
        let dir = dunce::canonicalize(&dir).ok()?;
        if !dir.starts_with(&self.workspace) {
            return None;
        }
        let out = dir.join(format!("{key:016x}.jpg"));
        if out.is_file() {
            return Some(out);
        }
        let _slot = self.posters.acquire().await.ok()?;
        if out.is_file() {
            return Some(out);
        }
        for at in ["1", "0"] {
            let args: Vec<OsString> = [
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-y",
                "-protocol_whitelist",
                "file",
                "-ss",
                at,
                "-i",
            ]
            .iter()
            .map(OsString::from)
            .chain([video.as_os_str().to_os_string()])
            .chain(
                ["-frames:v", "1", "-vf", "scale=360:-2", "-q:v", "5"]
                    .iter()
                    .map(OsString::from),
            )
            .chain([out.as_os_str().to_os_string()])
            .collect();
            let made = ferrule_gateway::tools::watch::run(
                sandbox,
                &ffmpeg.ffmpeg,
                &args,
                &self.workspace,
                Duration::from_secs(60),
            )
            .await;
            if made.is_ok() && out.is_file() {
                return Some(out);
            }
        }
        None
    }

    /// What the agent made, by workspace-relative path (with `/`).
    fn made(&self) -> HashMap<String, MediaJob> {
        let Some(jobs) = &self.jobs else {
            return HashMap::new();
        };
        let mut out = HashMap::new();
        for job in jobs.recent(2000).unwrap_or_default() {
            let p = Path::new(&job.out_path);
            let rel = p.strip_prefix(&self.workspace).unwrap_or(p);
            out.entry(slashed(rel)).or_insert(job);
        }
        out
    }
}

/// An upload's id: 32 hex digits.
fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `p` with `/` between its parts on every OS.
fn slashed(p: &Path) -> String {
    p.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

fn unix(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// What a file is, by its name: `photo`, `video`, `audio` or `other`.
fn kind_of(name: &str) -> &'static str {
    let mime = mime_for(name);
    match mime.split('/').next().unwrap_or("") {
        "image" => "photo",
        "video" => "video",
        "audio" => "audio",
        _ => "other",
    }
}

/// A made job's state, in the page's words.
fn state_words(job: &MediaJob) -> &'static str {
    match job.state.as_str() {
        "pending" => "rendering",
        "checking" => "made, not sent yet",
        "noted" => "finished, not checked",
        "unchecked" => "sent without a check",
        "failed" => "failed",
        "done" if job.channel.is_some() => "checked and sent",
        _ => "saved",
    }
}

fn library(ctx: &Ctx) -> Result<&Arc<Library>, Answer> {
    ctx.files.as_ref().ok_or_else(|| {
        bad(
            503,
            "the Files page works when the dashboard runs inside the gateway (the service)",
        )
    })
}

/// `GET /api/files?kind=&from=`: the library, newest first.
pub fn list(ctx: &Ctx, req: &Request) -> Answer {
    let lib = match library(ctx) {
        Ok(l) => l,
        Err(a) => return a,
    };
    let kind = req.query.get("kind").map(String::as_str).unwrap_or("");
    let from = req.query.get("from").map(String::as_str).unwrap_or("");
    let mut found = Vec::new();
    for root in ROOTS {
        walk(&lib.workspace, Path::new(root), 0, &mut found);
    }
    found.sort_by_key(|a| std::cmp::Reverse(a.2));
    let made = lib.made();
    let mut items = Vec::new();
    let mut total = 0usize;
    for (rel, size, modified) in found {
        let name = rel.rsplit('/').next().unwrap_or(&rel).to_string();
        let k = kind_of(&name);
        let mut parts = rel.split('/');
        let (top, second) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        let (source, channel) = match (top, second) {
            ("uploads", _) => ("upload", None),
            ("inbox", ch) => ("chat", Some(ch.to_string())),
            ("media", "screen") => ("screen", None),
            _ => ("made", None),
        };
        if (!kind.is_empty() && kind != k) || (!from.is_empty() && from != source) {
            continue;
        }
        total += 1;
        if items.len() >= LIST_MAX {
            continue;
        }
        let job = made.get(&rel).map(|j| {
            json!({
                "prompt": j.prompt,
                "model": j.model,
                "state": state_words(j),
                "cost_usd": j.cost_usd.or(j.estimate_usd),
                "estimated": j.cost_usd.is_none() && j.estimate_usd.is_some(),
            })
        });
        items.push(json!({
            "path": rel,
            "name": name,
            "size": size,
            "size_text": human(size),
            "modified": modified,
            "kind": k,
            "from": source,
            "channel": channel,
            "job": job,
        }));
    }
    ok(json!({
        "items": items,
        "total": total,
        "max_upload": lib.max_upload,
        "max_upload_text": human(lib.max_upload),
        "chunk": CHUNK,
        "posters": lib.video.is_some(),
        "chat": ctx.chat.as_ref().is_some_and(|c| c.listening()),
    }))
}

/// Files under `workspace/rel`, as (path with `/`, size, modified),
/// skipping hidden names (the part files among them) and symlinks.
fn walk(workspace: &Path, rel: &Path, depth: usize, out: &mut Vec<(String, u64, u64)>) {
    if depth > DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(workspace.join(rel)) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(e.path()) else {
            continue;
        };
        let here = rel.join(&name);
        if meta.is_dir() {
            walk(workspace, &here, depth + 1, out);
        } else if meta.is_file() {
            out.push((
                slashed(&here),
                meta.len(),
                meta.modified().map(unix).unwrap_or(0),
            ));
        }
    }
}

/// `GET /api/files/upload?id=`: how far an upload got.
pub fn upload_status(ctx: &Ctx, req: &Request) -> Answer {
    let lib = match library(ctx) {
        Ok(l) => l,
        Err(a) => return a,
    };
    let id = req.query.get("id").map(String::as_str).unwrap_or("");
    match lib.pending(id) {
        Ok((part, p)) => ok(json!({
            "offset": std::fs::metadata(part).map(|m| m.len()).unwrap_or(0),
            "size": p.size,
            "name": p.name,
        })),
        Err(why) => bad(404, why),
    }
}

/// `POST /api/files/upload/start {name, size, for?}`.
pub fn upload_start(ctx: &Ctx, body: &Value) -> Answer {
    let lib = match library(ctx) {
        Ok(l) => l,
        Err(a) => return a,
    };
    let name = safe_name(body["name"].as_str().unwrap_or(""));
    let size = body["size"].as_u64().unwrap_or(0);
    if size == 0 {
        return bad(400, "That file is empty.");
    }
    if size > lib.max_upload {
        return bad(
            413,
            format!(
                "That file is {}, over the {} the page takes ([dashboard] max_upload_mb).",
                human(size),
                human(lib.max_upload)
            ),
        );
    }
    let dir = match lib.parts() {
        Ok(d) => d,
        Err(why) => return bad(500, why),
    };
    if let Some(free) = crate::selfcheck::free_space(&dir) {
        if free < size.saturating_add(FREE_FLOOR) {
            return bad(
                507,
                format!(
                    "There's {} free on the disk; this file would leave less than 1 GB.",
                    human(free)
                ),
            );
        }
    }
    let ticket = body["for"]
        .as_str()
        .filter(|t| lib.known_ticket(t))
        .map(str::to_string);
    let id = uuid::Uuid::new_v4().simple().to_string();
    let note = Pending {
        name: name.clone(),
        size,
        ticket,
    };
    let made = serde_json::to_vec(&note)
        .map_err(|e| e.to_string())
        .and_then(|bytes| {
            std::fs::write(dir.join(format!("{id}.json")), bytes).map_err(|e| e.to_string())
        })
        .and_then(|()| std::fs::File::create(dir.join(&id)).map_err(|e| e.to_string()));
    if let Err(e) = made {
        return bad(500, format!("The upload couldn't start: {e}"));
    }
    ok(json!({ "id": id, "offset": 0, "chunk": CHUNK, "name": name }))
}

/// `PUT /api/files/upload/<id>?offset=<n>`: one piece, appended. The
/// caller has checked the session, the origin and the CSRF header.
pub async fn put_chunk(ctx: &Ctx, id: &str, req: &Request) -> Response {
    let Some(lib) = &ctx.files else {
        return Response::json(
            503,
            &json!({ "error": "the Files page works when the dashboard runs inside the gateway (the service)" }),
        );
    };
    let Some(offset) = req.query.get("offset").and_then(|o| o.parse::<u64>().ok()) else {
        return Response::json(
            400,
            &json!({ "error": "which offset this piece starts at is missing" }),
        );
    };
    let _one = lib.writing.lock().await;
    let (part, pending) = match lib.pending(id) {
        Ok(p) => p,
        Err(why) => return Response::json(404, &json!({ "error": why })),
    };
    let have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    if offset != have {
        return Response::json(
            409,
            &json!({ "error": "that piece isn't the next one", "offset": have }),
        );
    }
    let len = req.body.len() as u64;
    if have + len > pending.size {
        return Response::json(
            400,
            &json!({ "error": "that's more than the file's size", "offset": have }),
        );
    }
    let appended = async {
        use tokio::io::AsyncWriteExt;
        let mut f = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&part)
            .await?;
        f.write_all(&req.body).await?;
        f.flush().await
    }
    .await;
    match appended {
        Ok(()) => Response::json(200, &json!({ "offset": have + len })),
        Err(e) => {
            // A half-written piece is cut back, so the offset stays true.
            if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&part) {
                let _ = f.set_len(have);
            }
            Response::json(
                500,
                &json!({ "error": format!("the piece couldn't be written: {e}"), "offset": have }),
            )
        }
    }
}

/// `uploads/<day>/<name>`, or `<stem>-2.<ext>` and on when it's taken.
fn free_name(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let p = Path::new(name);
    let stem = p
        .file_stem()
        .map_or_else(|| name.to_string(), |s| s.to_string_lossy().into_owned());
    let ext = p
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (2..10_000)
        .map(|n| dir.join(format!("{stem}-{n}{ext}")))
        .find(|c| !c.exists())
        .unwrap_or_else(|| dir.join(format!("{}-{name}", uuid::Uuid::new_v4().simple())))
}

/// `POST /api/files/upload/finish {id}`: the whole file, moved into
/// `uploads/<day>/`; the chat that asked for it is woken.
pub async fn upload_finish(ctx: &Ctx, body: &Value) -> Answer {
    let lib = match library(ctx) {
        Ok(l) => l,
        Err(a) => return a,
    };
    let id = body["id"].as_str().unwrap_or("");
    let _one = lib.writing.lock().await;
    let (part, pending) = match lib.pending(id) {
        Ok(p) => p,
        Err(why) => return bad(404, why),
    };
    let have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    if have != pending.size {
        return Some((
            409,
            json!({
                "error": format!("It has {} of {}: the rest has to go up first.", human(have), human(pending.size)),
                "offset": have,
            }),
        ));
    }
    let day = chrono::Local::now().format("%Y-%m-%d").to_string();
    let dir = lib.workspace.join("uploads").join(&day);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return bad(500, format!("uploads/{day} can't be made: {e}"));
    }
    let dest = free_name(&dir, &pending.name);
    if let Err(e) = std::fs::rename(&part, &dest) {
        return bad(500, format!("The file couldn't be moved into uploads: {e}"));
    }
    let _ = std::fs::remove_file(part.with_extension("json"));
    let rel = slashed(dest.strip_prefix(&lib.workspace).unwrap_or(&dest));
    // The chat whose too-big file this is carries on with it.
    let woke = pending
        .ticket
        .as_deref()
        .and_then(|t| lib.take_ticket(t))
        .map(|session| {
            let router = ctx.live.as_ref().and_then(|l| l.router.upgrade());
            router.is_some_and(|r| {
                r.can_wake(&session)
                    && r.wake(
                        &session,
                        format!(
                            "[ferrule] The owner uploaded the file that was too big to send in this chat, through the dashboard: {rel} ({}). Carry on with what they asked for it.",
                            human(have)
                        ),
                    )
            })
        });
    ok(json!({ "path": rel, "size": have, "size_text": human(have), "woke": woke }))
}

/// `POST /api/files/delete {path}`: into `.ferrule/trash/`, for 7 days.
pub fn delete(ctx: &Ctx, body: &Value) -> Answer {
    let lib = match library(ctx) {
        Ok(l) => l,
        Err(a) => return a,
    };
    let path = match lib.inside(body["path"].as_str().unwrap_or("")) {
        Ok(p) => p,
        Err(why) => return bad(404, why),
    };
    let trash = lib.workspace.join(TRASH);
    if let Err(e) = std::fs::create_dir_all(&trash) {
        return bad(500, format!("{TRASH} can't be made: {e}"));
    }
    let name = path
        .file_name()
        .map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
    let dest = free_name(&trash, &format!("{}-{name}", unix(SystemTime::now())));
    match std::fs::rename(&path, &dest) {
        Ok(()) => ok(json!({
            "ok": true,
            "said": format!("{name} is in the trash; housekeeping removes it for good after {} days.", crate::housekeeping::TRASH_DAYS),
        })),
        Err(e) => bad(500, format!("{name} couldn't be moved to the trash: {e}")),
    }
}

/// `POST /api/files/say {path, text}`: the file and the owner's words to
/// the page's chat.
pub async fn say(ctx: &Ctx, body: &Value) -> Answer {
    let lib = match library(ctx) {
        Ok(l) => l,
        Err(a) => return a,
    };
    let text = body["text"].as_str().unwrap_or("");
    if text.len() > MAX_TEXT {
        return bad(413, format!("that message is over {} KB", MAX_TEXT / 1024));
    }
    let path = match lib.inside(body["path"].as_str().unwrap_or("")) {
        Ok(p) => p,
        Err(why) => return bad(404, why),
    };
    let Some(ch) = &ctx.chat else {
        return bad(
            503,
            "chat from the page works when the dashboard runs inside the gateway (the service)",
        );
    };
    let name = path
        .file_name()
        .map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
    let saved = files::Saved {
        rel: slashed(path.strip_prefix(&lib.workspace).unwrap_or(&path)),
        bytes: std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
        mime: mime_for(&name).to_string(),
        path,
    };
    match ch.say_file(text, saved).await {
        Ok(()) => ok(json!({ "ok": true })),
        Err(why) => bad(503, why),
    }
}

/// A `Range` header against a file of `total` bytes: `Ok(None)` for the
/// whole file (none, another unit, several ranges), `Err` when it can't
/// be met.
fn range(header: Option<&str>, total: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(spec) = header.and_then(|h| h.trim().strip_prefix("bytes=")) else {
        return Ok(None);
    };
    if spec.contains(',') {
        return Ok(None);
    }
    let (a, b) = spec.split_once('-').ok_or(())?;
    if total == 0 {
        return Err(());
    }
    let n = |s: &str| s.trim().parse::<u64>().map_err(|_| ());
    match (a.trim(), b.trim()) {
        ("", last) => {
            let last = n(last)?;
            if last == 0 {
                return Err(());
            }
            Ok(Some((total.saturating_sub(last), total - 1)))
        }
        (first, "") => {
            let first = n(first)?;
            if first >= total {
                return Err(());
            }
            Ok(Some((first, total - 1)))
        }
        (first, last) => {
            let (first, last) = (n(first)?, n(last)?);
            if first > last || first >= total {
                return Err(());
            }
            Ok(Some((first, last.min(total - 1))))
        }
    }
}

/// A file name for `Content-Disposition`: printable ASCII, no quotes.
fn header_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.trim().is_empty() {
        "file".into()
    } else {
        s
    }
}

/// `GET /api/files/raw?path=&download=`: one file of the library.
pub fn raw(ctx: &Ctx, req: &Request) -> Response {
    let Some(lib) = &ctx.files else {
        return Response::json(
            503,
            &json!({ "error": "the Files page works when the dashboard runs inside the gateway (the service)" }),
        );
    };
    let path = match lib.inside(req.query.get("path").map(String::as_str).unwrap_or("")) {
        Ok(p) => p,
        Err(why) => return Response::json(404, &json!({ "error": why })),
    };
    let name = path
        .file_name()
        .map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
    let mime = mime_for(&name);
    // `mime_for` names only the image types a browser shows safely
    // (never SVG): those, video and audio play here; the rest download.
    let plays = ["image/", "video/", "audio/"]
        .iter()
        .any(|p| mime.starts_with(p));
    let inline = plays && !req.query.contains_key("download");
    let total = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let mut resp = Response::file(
        path,
        if plays {
            mime
        } else {
            "application/octet-stream"
        },
    );
    match range(req.header("range"), total) {
        Ok(None) => {}
        Ok(Some((first, last))) => resp = resp.with_range(first, last),
        Err(()) => {
            return Response::new(416, "text/plain; charset=utf-8", "")
                .with_header("Content-Range", format!("bytes */{total}"))
        }
    }
    resp.with_header("Accept-Ranges", "bytes".into())
        .with_header(
            "Content-Disposition",
            format!(
                "{}; filename=\"{}\"",
                if inline { "inline" } else { "attachment" },
                header_name(&name)
            ),
        )
}

/// `GET /api/files/thumb?path=`: an image itself, a video's poster.
pub async fn thumb(ctx: &Ctx, req: &Request) -> Response {
    let Some(lib) = &ctx.files else {
        return Response::text(404, "no poster");
    };
    let Ok(path) = lib.inside(req.query.get("path").map(String::as_str).unwrap_or("")) else {
        return Response::text(404, "no poster");
    };
    let name = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let mime = mime_for(&name);
    if mime.starts_with("image/") {
        return Response::file(path, mime);
    }
    if mime.starts_with("video/") {
        if let Some(poster) = lib.poster(&path).await {
            return Response::file(poster, "image/jpeg");
        }
    }
    Response::text(404, "no poster")
}

/// A file too big for the owner's own chat: they get a one-time login
/// link to the Files page, tied to that chat, and the message still goes
/// on to the agent (which reads that the file wasn't saved). The link is
/// only ever this message from the gateway, never in a transcript.
pub struct TooBigDoor {
    pub dash: Arc<super::Dashboard>,
    pub hub: Arc<ferrule_trust::Hub>,
}

#[async_trait::async_trait]
impl ferrule_gateway::Interceptor for TooBigDoor {
    async fn intercept(&self, msg: &ferrule_gateway::InboundMessage) -> Option<String> {
        if !files::has_too_big(&msg.text) || crate::trust::owner_in(&self.hub, msg) != Some(true) {
            return None;
        }
        let lib = self.dash.ctx.files.clone()?;
        let to = self.hub.owner_on(&msg.channel)?;
        let session = ferrule_gateway::session::session_id(&msg.channel, &msg.chat_id);
        let ticket = lib.ticket(&session);
        let lead = format!(
            "That file is bigger than this chat can bring in. Upload it on the dashboard's Files page instead (up to {}); when it's up, this chat is told where it is and carries on.",
            human(lib.max_upload)
        );
        let (dash, hub) = (self.dash.clone(), self.hub.clone());
        tokio::spawn(async move {
            let suffix = format!("?for={ticket}");
            if let Err(e) = super::door::send_link_with(&dash, &hub, &to, &suffix, &lead).await {
                hub.tell_in(&to, format!("No upload link: {e}"));
            }
        });
        None
    }
}

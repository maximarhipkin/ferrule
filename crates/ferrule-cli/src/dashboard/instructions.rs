//! M66: the standing instructions every new conversation reads (the
//! context baseline, `ferrule_core::baseline`): AGENTS.md, CLAUDE.md,
//! GEMINI.md and ferrule.md beside the config (every workspace) and in the
//! workspace. The page shows them and saves one; the prompt is built as
//! before, so a running conversation keeps the prefix it started with.

use super::api::{arg, bad, by, missing, need, ok, Answer};
use super::Ctx;
use ferrule_core::baseline::BASELINE_FILES;
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

/// The most a layer may be saved at.
pub const MAX_BYTES: usize = 64 * 1024;
/// The most a save's request may weigh: 64 KB of text escaped as JSON
/// (at most six bytes a byte, `\u0001`), so the owner meets this route's
/// own 413, not the server's.
pub const MAX_BODY: usize = 6 * MAX_BYTES + 4096;
/// Said of a file that holds a key: the page never gets its text.
const HOLDS_A_KEY: &str =
    "it holds a key or token: edit it on the machine, and keep keys in the Safe";
/// The most of a file the page is shown.
const MAX_SHOWN: u64 = 256 * 1024;

pub const NOTE: &str = "New conversations read these when they start: the file beside the config (for every workspace), then any in the workspace's parent folders, then the workspace's own. In each folder only the first of AGENTS.md, CLAUDE.md, GEMINI.md and ferrule.md that exists is read (an empty one counts, and adds nothing), up to 16,000 characters in all. A change applies to new conversations; ones already running keep theirs.";

struct Layer {
    id: String,
    label: String,
    path: PathBuf,
    scope: &'static str,
    /// The first file of its folder: the one that's read.
    active: bool,
}

/// The workspace the agent reads them in: the gateway's, else the config's
/// local one, else this process's folder. `Err`: a remote workspace,
/// whose files are read over SSH.
fn workspace(ctx: &Ctx) -> Result<Option<PathBuf>, String> {
    if let Some(files) = &ctx.files {
        return Ok(Some(files.workspace().to_path_buf()));
    }
    if let Some(cfg) = super::api::config(ctx) {
        if let Some(spec) = cfg.workspace.as_deref() {
            if ferrule_ssh::is_remote(spec) {
                return Err(spec.to_string());
            }
            return Ok(Some(PathBuf::from(spec)));
        }
    }
    Ok(ctx.workspace.clone())
}

fn layers(ctx: &Ctx) -> (Vec<Layer>, Option<String>) {
    let mut out = Vec::new();
    let mut note = None;
    let canon = |p: &Path| dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let global = ctx
        .config_path
        .as_deref()
        .and_then(Path::parent)
        .filter(|d| !d.as_os_str().is_empty())
        .map(canon);
    let ws = match workspace(ctx) {
        Ok(w) => w.map(|w| canon(&w)),
        Err(spec) => {
            note = Some(format!(
                "The workspace is remote ({spec}): its own files are read there, over SSH, and aren't shown here."
            ));
            None
        }
    };
    let mut dirs: Vec<(&'static str, PathBuf)> = Vec::new();
    if let Some(g) = global.filter(|g| Some(g) != ws.as_ref()) {
        dirs.push(("global", g));
    }
    if let Some(w) = ws {
        dirs.push(("workspace", w));
    }
    for (scope, dir) in dirs {
        let mut seen_first = false;
        for name in BASELINE_FILES {
            let path = dir.join(name);
            let readable = path.is_file();
            let active = readable && !seen_first;
            seen_first |= readable;
            out.push(Layer {
                id: format!("{scope}:{name}"),
                label: match scope {
                    "global" => format!("{name} beside the config (every workspace)"),
                    _ => format!("{name} in the workspace"),
                },
                path,
                scope,
                active,
            });
        }
    }
    (out, note)
}

/// The hash a save must send back: of the bytes on disk (none: of nothing).
fn hash_of(path: &Path) -> String {
    let bytes = std::fs::read(path).unwrap_or_default();
    ferrule_connections::seal::sha256_b64(&bytes)
}

/// Whether `text` holds a key: a value this machine keeps, or one's shape.
fn holds_a_key(ctx: &Ctx, text: &str) -> bool {
    ferrule_gateway::paste_guard::holds_a_key(text, &ctx.redactor)
}

/// Whether the page may write `layer`, and if not, why.
fn writable(layer: &Layer) -> Result<(), String> {
    if !layer.path.parent().is_some_and(Path::is_dir) {
        return Err("its folder doesn't exist".into());
    }
    match std::fs::symlink_metadata(&layer.path) {
        Ok(m) if m.file_type().is_symlink() => {
            Err("it's a link: edit the file it points to on the machine".into())
        }
        Ok(m) if !m.is_file() => Err("it isn't a file".into()),
        Ok(m) if m.len() > MAX_BYTES as u64 => Err(format!(
            "it's over {} KB: edit it on the machine",
            MAX_BYTES / 1024
        )),
        _ => Ok(()),
    }
}

/// `GET /api/instructions`.
pub fn list(ctx: &Ctx) -> Answer {
    let (all, remote) = layers(ctx);
    let rows: Vec<Value> = all
        .iter()
        .map(|l| {
            let meta = std::fs::metadata(&l.path).ok().filter(|m| m.is_file());
            let bytes = meta.as_ref().map_or(0, |m| m.len());
            let mut text = if bytes <= MAX_SHOWN {
                meta.and_then(|_| std::fs::read_to_string(&l.path).ok())
                    .unwrap_or_default()
            } else {
                String::new()
            };
            let mut can = writable(l);
            // Redacted, a save would write the placeholder back over the
            // key: such a file is neither shown nor saved from here.
            if holds_a_key(ctx, &text) {
                text.clear();
                can = Err(HOLDS_A_KEY.into());
            }
            json!({
                "id": l.id,
                "label": l.label,
                "path": l.path.display().to_string(),
                "scope": l.scope,
                "exists": l.path.is_file(),
                "active": l.active,
                "text": ctx.redactor.redact(&text),
                "bytes": bytes,
                "editable": can.is_ok(),
                "why": can.err(),
                // Sent back with a save: a file changed since is refused.
                "hash": hash_of(&l.path),
            })
        })
        .collect();
    let note = match remote {
        Some(r) => format!("{NOTE} {r}"),
        None => NOTE.to_string(),
    };
    ok(json!({ "layers": rows, "note": note }))
}

/// Writes `text` to `path` whole or not at all: a new file beside it,
/// renamed over it, with the old file's permissions.
fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no folder"))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(text.as_bytes())?;
    tmp.as_file().sync_all()?;
    match std::fs::metadata(path) {
        Ok(m) => std::fs::set_permissions(tmp.path(), m.permissions())?,
        #[cfg(unix)]
        Err(_) => {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o644))?
        }
        #[cfg(not(unix))]
        Err(_) => {}
    }
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// `POST /api/instructions/save {id, text, base_hash}`: only one of the
/// layers listed, unchanged since the page read it, at most 64 KB, with no
/// key in it (it goes into every new conversation's prompt).
pub fn save(ctx: &Ctx, body: &Value) -> Answer {
    let id = need!(arg(body, "id"));
    let Some(text) = body.get("text").and_then(Value::as_str) else {
        return bad(400, "`text` is missing");
    };
    if text.len() > MAX_BYTES {
        return bad(
            413,
            format!(
                "That's over {} KB; instructions this long would crowd the conversation out. Nothing was saved.",
                MAX_BYTES / 1024
            ),
        );
    }
    let Some(base) = body.get("base_hash").and_then(Value::as_str) else {
        return bad(
            400,
            "`base_hash` is missing: reload the page, then save again. Nothing was saved.",
        );
    };
    if holds_a_key(ctx, text) {
        return bad(
            400,
            "That text holds what looks like a key or token, and these instructions go into every new conversation's prompt. Take it out (keys belong in the Safe); nothing was saved.",
        );
    }
    let (all, _) = layers(ctx);
    let Some(layer) = all.iter().find(|l| l.id == id) else {
        return bad(
            404,
            format!("There's no instructions file `{id}`; the page lists the ones it can save."),
        );
    };
    let now = std::fs::read_to_string(&layer.path).unwrap_or_default();
    if let Err(why) = writable(layer).and_then(|()| {
        if holds_a_key(ctx, &now) {
            Err(HOLDS_A_KEY.to_string())
        } else {
            Ok(())
        }
    }) {
        return bad(
            409,
            format!("{} can't be saved from here: {why}.", layer.path.display()),
        );
    }
    if hash_of(&layer.path) != base {
        return Some((
            409,
            json!({
                "error": format!(
                    "{} changed since this page loaded it (on the machine, in another tab, or by the agent). Nothing was saved: reload to see it as it is now, then make your change again.",
                    layer.path.display()
                ),
                "stale": true,
            }),
        ));
    }
    let Some(hub) = &ctx.hub else {
        return missing("the trust hub (every save is audited)");
    };
    if let Err(e) = write_atomic(&layer.path, text) {
        return bad(
            500,
            format!(
                "{} wasn't saved: {e}. It is as it was.",
                layer.path.display()
            ),
        );
    }
    hub.audit().record(
        chrono::Utc::now(),
        "dashboard.instructions.saved",
        None,
        None,
        json!({
            "by": by(),
            "layer": layer.id,
            "path": layer.path.display().to_string(),
            "bytes": text.len(),
            "sha256": ferrule_trust::plan::sha256_hex(text),
        }),
    );
    ok(json!({
        "ok": true,
        "said": "Saved. New conversations use it; ones already running keep theirs.",
    }))
}

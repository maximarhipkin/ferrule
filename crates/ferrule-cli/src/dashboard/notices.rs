//! M37 §1: the problems banner's strips close. A dismissal is kept on the
//! server, per owner, in `<data>/dashboard/notices.json`, so it holds
//! across a reload, another device and a restart. A hidden notice that is
//! still true comes back after [`HIDE_FOR`], saying so; one whose text
//! changed is a new notice. The kill switch and "no model can answer" never
//! close.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// How long a closed notice stays closed while it's still true.
pub const HIDE_FOR: u64 = 24 * 3600;
/// An entry is forgotten this long after it was made.
const FORGET_AFTER: u64 = 7 * 24 * 3600;

/// Ids that can't be hidden, and why.
pub fn unclosable(id: &str) -> Option<&'static str> {
    match id {
        "kill" => Some("the kill switch stops every run; it shows until it's off"),
        "models-none" => Some("no model can answer; it shows until one can"),
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    at: u64,
    until: u64,
    /// The notice's text, hashed: another text is another notice.
    what: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    v: u32,
    #[serde(default)]
    owners: BTreeMap<String, BTreeMap<String, Entry>>,
}

pub struct Notices {
    path: PathBuf,
}

/// Every request opens its own [`Notices`]; one read-modify-write at a time.
static LOCK: Mutex<()> = Mutex::new(());

/// 16 hex characters of SHA-256.
pub fn hash(text: &str) -> String {
    let d = ring::digest::digest(&ring::digest::SHA256, text.as_bytes());
    d.as_ref()[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// A problem's id: its own, or `h:` + the hash of its section and text.
pub fn id_of(p: &Value) -> String {
    if let Some(id) = p["id"].as_str() {
        return id.to_string();
    }
    let section = p["section"].as_str().unwrap_or("");
    let what = p["what"].as_str().unwrap_or("");
    format!("h:{}", hash(&format!("{section}\n{what}")))
}

fn what_of(p: &Value) -> String {
    hash(p["what"].as_str().unwrap_or(""))
}

impl Notices {
    pub fn at(dir: &Path) -> Self {
        Self {
            path: dir.join("dashboard").join("notices.json"),
        }
    }

    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A file that doesn't read counts as empty: nothing hidden.
    fn load(&self) -> File {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn save(&self, file: &File) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        crate::secrets::write_private(&self.path, &serde_json::to_string_pretty(file)?)
    }

    /// Splits `problems` into what shows and what's hidden. Every problem
    /// gets its `id` and `closable`; one that's back after its day hidden
    /// gets `back` (when it was closed).
    pub fn split(&self, owner: &str, problems: Vec<Value>, now: u64) -> (Vec<Value>, Vec<Value>) {
        let file = self.load();
        let mine = file.owners.get(owner);
        let mut shown = Vec::new();
        let mut hidden = Vec::new();
        for mut p in problems {
            let id = id_of(&p);
            p["id"] = json!(id);
            p["closable"] = json!(unclosable(&id).is_none());
            let entry = mine
                .and_then(|m| m.get(&id))
                .filter(|e| e.what == what_of(&p) && unclosable(&id).is_none());
            match entry {
                Some(e) if e.until > now => {
                    p["until"] = json!(e.until);
                    hidden.push(p);
                }
                Some(e) => {
                    p["back"] = json!(e.at);
                    shown.push(p);
                }
                None => shown.push(p),
            }
        }
        (shown, hidden)
    }

    /// Hides `problem` (which must be one of today's) for [`HIDE_FOR`].
    pub fn dismiss(&self, owner: &str, problem: &Value, now: u64) -> Result<u64, String> {
        let id = id_of(problem);
        if let Some(why) = unclosable(&id) {
            return Err(format!("this one can't be hidden: {why}"));
        }
        let _held = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut file = self.load();
        file.v = 1;
        prune(&mut file, now);
        let until = now + HIDE_FOR;
        file.owners.entry(owner.to_string()).or_default().insert(
            id,
            Entry {
                at: now,
                until,
                what: what_of(problem),
            },
        );
        self.save(&file).map_err(|e| format!("{e:#}"))?;
        Ok(until)
    }

    /// Shows `id` again, or every hidden notice; how many came back.
    pub fn restore(&self, owner: &str, id: Option<&str>, now: u64) -> Result<usize, String> {
        let _held = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut file = self.load();
        file.v = 1;
        prune(&mut file, now);
        let n = match (file.owners.get_mut(owner), id) {
            (None, _) => 0,
            (Some(m), Some(id)) => usize::from(m.remove(id).is_some()),
            (Some(m), None) => {
                let n = m.len();
                m.clear();
                n
            }
        };
        file.owners.retain(|_, m| !m.is_empty());
        self.save(&file).map_err(|e| format!("{e:#}"))?;
        Ok(n)
    }
}

fn prune(file: &mut File, now: u64) {
    for m in file.owners.values_mut() {
        m.retain(|_, e| e.at + FORGET_AFTER > now);
    }
    file.owners.retain(|_, m| !m.is_empty());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(id: Option<&str>, what: &str) -> Value {
        let mut v = json!({ "what": what, "section": "health" });
        if let Some(id) = id {
            v["id"] = json!(id);
        }
        v
    }

    #[test]
    fn a_closed_notice_stays_closed_for_a_day_then_comes_back_saying_so() {
        let dir = tempfile::tempdir().unwrap();
        let n = Notices::at(dir.path());
        let stale = p(Some("channel:telegram"), "telegram hasn't polled");
        let t0 = 1_800_000_000;
        let until = n.dismiss("owner", &stale, t0).unwrap();
        assert_eq!(until, t0 + HIDE_FOR);

        // Another process (a restart, another device) sees the same file.
        let again = Notices::at(dir.path());
        let (shown, hidden) = again.split("owner", vec![stale.clone()], t0 + 60);
        assert!(shown.is_empty());
        assert_eq!(hidden[0]["id"], "channel:telegram");
        assert_eq!(hidden[0]["until"], until);

        // Still true a day later: back, with when it was closed.
        let (shown, hidden) = again.split("owner", vec![stale.clone()], t0 + HIDE_FOR + 1);
        assert!(hidden.is_empty());
        assert_eq!(shown[0]["back"], t0);
        assert_eq!(shown[0]["closable"], true);
    }

    #[test]
    fn another_text_is_another_notice_and_another_owner_sees_their_own() {
        let dir = tempfile::tempdir().unwrap();
        let n = Notices::at(dir.path());
        let t0 = 1_800_000_000;
        n.dismiss("a", &p(Some("model-down:x"), "x is failing: 500"), t0)
            .unwrap();
        let (shown, _) = n.split("a", vec![p(Some("model-down:x"), "x is failing: 429")], t0);
        assert_eq!(shown.len(), 1, "a new error shows");
        let (shown, _) = n.split("b", vec![p(Some("model-down:x"), "x is failing: 500")], t0);
        assert_eq!(shown.len(), 1, "b never closed it");
        let (shown, hidden) = n.split("a", vec![p(Some("model-down:x"), "x is failing: 500")], t0);
        assert!(shown.is_empty() && hidden.len() == 1);
    }

    #[test]
    fn the_kill_switch_and_no_model_can_answer_never_close() {
        let dir = tempfile::tempdir().unwrap();
        let n = Notices::at(dir.path());
        for id in ["kill", "models-none"] {
            let e = n.dismiss("owner", &p(Some(id), "…"), 1).unwrap_err();
            assert!(e.contains("can't be hidden"), "{e}");
            let (shown, _) = n.split("owner", vec![p(Some(id), "…")], 1);
            assert_eq!(shown[0]["closable"], false);
        }
    }

    #[test]
    fn restore_brings_one_or_all_back_and_a_broken_file_hides_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let n = Notices::at(dir.path());
        let a = p(None, "first");
        let b = p(None, "second");
        assert!(id_of(&a).starts_with("h:") && id_of(&a) != id_of(&b));
        n.dismiss("o", &a, 10).unwrap();
        n.dismiss("o", &b, 10).unwrap();
        assert_eq!(n.restore("o", Some(&id_of(&a)), 11).unwrap(), 1);
        let (shown, hidden) = n.split("o", vec![a.clone(), b.clone()], 12);
        assert_eq!((shown.len(), hidden.len()), (1, 1));
        assert_eq!(n.restore("o", None, 12).unwrap(), 1);
        let (shown, _) = n.split("o", vec![a.clone(), b.clone()], 12);
        assert_eq!(shown.len(), 2);

        n.dismiss("o", &a, 10).unwrap();
        std::fs::write(n.path(), "{ not json").unwrap();
        let (shown, _) = n.split("o", vec![a.clone()], 12);
        assert_eq!(shown.len(), 1);
        n.dismiss("o", &a, 13).unwrap();
        let (shown, _) = n.split("o", vec![a], 14);
        assert!(shown.is_empty(), "rewritten on the next dismissal");
    }

    #[test]
    fn entries_are_forgotten_after_a_week() {
        let dir = tempfile::tempdir().unwrap();
        let n = Notices::at(dir.path());
        n.dismiss("o", &p(Some("x"), "x"), 10).unwrap();
        n.dismiss("o", &p(Some("y"), "y"), 10 + FORGET_AFTER + 1)
            .unwrap();
        let text = std::fs::read_to_string(n.path()).unwrap();
        assert!(!text.contains("\"x\"") && text.contains("\"y\""), "{text}");
    }
}

//! The approval queue: one JSON file per request under
//! `<data>/private/extensions/pending/`, which the model's sandbox can't
//! read or write. The owner clears it with `ferrule extensions
//! approve|deny`; nothing in a request has been fetched or run.

use crate::error::Result;
use crate::scan::Finding;
use crate::source::{McpRequest, PluginRequest, SkillRequest};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Request {
    Mcp(McpRequest),
    Skill(SkillRequest),
    Plugin(PluginRequest),
}

impl Request {
    pub fn source(&self) -> &str {
        match self {
            Request::Mcp(r) => &r.source,
            Request::Skill(r) => &r.source,
            Request::Plugin(r) => &r.source,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Request::Mcp(r) => {
                let mut s = format!("MCP server `{}` from {}", r.name, r.source);
                if let Some(c) = &r.command {
                    s.push_str(&format!(", command `{c}`"));
                }
                if !r.args.is_empty() {
                    s.push_str(&format!(", args {:?}", r.args));
                }
                s
            }
            Request::Skill(r) => match &r.path {
                Some(p) => format!("skill from {} (path `{p}`)", r.source),
                None => format!("skill from {}", r.source),
            },
            Request::Plugin(r) => {
                let mut s = format!("WASM plugin from {}", r.source);
                if let Some(p) = &r.path {
                    s.push_str(&format!(" (path `{p}`)"));
                }
                if let Some(c) = &r.capabilities {
                    s.push_str(&format!(", allowed to: {}", c.describe().join("; ")));
                }
                s
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub id: String,
    pub created_at: String,
    /// Why it wasn't installed straight away.
    pub reason: String,
    pub request: Request,
    /// Scan hits when the request was queued because of them; shown to the
    /// owner at approval, never to the model.
    #[serde(default)]
    pub findings: Vec<Finding>,
}

#[derive(Debug, Clone)]
pub struct PendingQueue {
    dir: PathBuf,
}

impl PendingQueue {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn add(&self, request: Request, reason: &str, findings: Vec<Finding>) -> Result<Pending> {
        create_private_dir(&self.dir)?;
        let id = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
        let p = Pending {
            id: id.clone(),
            created_at: crate::lock::now(),
            reason: reason.into(),
            request,
            findings,
        };
        let mut tmp = tempfile::NamedTempFile::new_in(&self.dir)?;
        serde_json::to_writer_pretty(&mut tmp, &p)?;
        tmp.persist(self.path(&id)).map_err(|e| e.error)?;
        Ok(p)
    }

    /// Oldest first. Unreadable files are skipped with a warning.
    pub fn list(&self) -> Result<Vec<Pending>> {
        let rd = match fs::read_dir(&self.dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            match fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|t| serde_json::from_str::<Pending>(&t).map_err(|e| e.to_string()))
            {
                Ok(p) => out.push(p),
                Err(e) => tracing::warn!(path = %path.display(), "unreadable pending request: {e}"),
            }
        }
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        Ok(out)
    }

    pub fn get(&self, id: &str) -> Result<Option<Pending>> {
        if !valid_id(id) {
            return Ok(None);
        }
        match fs::read_to_string(self.path(id)) {
            Ok(t) => Ok(Some(serde_json::from_str(&t)?)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// True if there was such a request.
    pub fn remove(&self, id: &str) -> Result<bool> {
        if !valid_id(id) {
            return Ok(false);
        }
        match fs::remove_file(self.path(id)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 32 && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Owner-only on Unix. The sandbox is what keeps the agent out; this keeps
/// other local users out.
pub(crate) fn create_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(name: &str) -> Request {
        Request::Mcp(McpRequest {
            name: name.into(),
            source: "npm:x@1.0.0".into(),
            command: None,
            args: vec![],
            replace: false,
        })
    }

    #[test]
    fn add_list_get_remove() {
        let tmp = tempfile::tempdir().unwrap();
        let q = PendingQueue::new(tmp.path().join("private/extensions/pending"));
        assert!(q.list().unwrap().is_empty());
        let a = q.add(req("a"), "not on the allow-list", vec![]).unwrap();
        let b = q.add(req("b"), "not on the allow-list", vec![]).unwrap();
        let ids: Vec<_> = q.list().unwrap().into_iter().map(|p| p.id).collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&a.id) && ids.contains(&b.id));
        assert_eq!(q.get(&a.id).unwrap().unwrap().request, req("a"));
        assert!(q.remove(&a.id).unwrap());
        assert!(!q.remove(&a.id).unwrap());
        assert!(q.get(&a.id).unwrap().is_none());
        assert!(q.get("../../etc/passwd").unwrap().is_none());
        assert!(!q.remove("../x").unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(tmp.path().join("private/extensions/pending"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700);
        }
    }
}

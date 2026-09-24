//! Plans proposed in plan mode: `<data>/plans/<id>.json`.

use ring::digest::{digest, SHA256};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Proposed,
    Approved,
    Rejected,
    /// Approved and carried out (by the run in `executed_by`).
    Executed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub id: String,
    /// RFC 3339, UTC.
    pub created: String,
    pub task: String,
    pub workspace: String,
    pub session: String,
    pub text: String,
    /// SHA-256 of `text`, hex.
    pub sha256: String,
    pub status: PlanStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executed_by: Option<String>,
}

pub fn sha256_hex(text: &str) -> String {
    digest(&SHA256, text.as_bytes())
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The first message of the run that carries out an approved plan.
pub fn execution_prompt(plan: &Plan) -> String {
    format!(
        "The owner approved this plan (sha256 {}). Carry it out; if you depart from it, say where and why in your answer.\n\n[Approved plan]\n{}\n\n[Original task]\n{}",
        &plan.sha256[..12],
        plan.text.trim(),
        plan.task.trim()
    )
}

/// What plan mode tells the model its job is.
pub const PLAN_MODE_NOTE: &str = "[Plan mode] Explore read-only: read files, list directories, search, fetch. Nothing that changes files or the world will run. When you understand the task, answer with a numbered plan: the steps, the files each touches, the commands you'd run (say which are destructive), and how you'll check the result. The plan is your whole answer; the owner approves it before anything runs.";

pub struct PlanStore {
    dir: PathBuf,
}

impl PlanStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn path(&self, id: &str) -> Result<PathBuf, String> {
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(format!("`{id}` isn't a plan id"));
        }
        Ok(self.dir.join(format!("{id}.json")))
    }

    pub fn propose(
        &self,
        task: &str,
        workspace: &Path,
        session: &str,
        text: &str,
    ) -> std::io::Result<Plan> {
        let plan = Plan {
            id: uuid::Uuid::new_v4().simple().to_string()[..8].to_string(),
            created: chrono::Utc::now().to_rfc3339(),
            task: task.into(),
            workspace: workspace.display().to_string(),
            session: session.into(),
            text: text.into(),
            sha256: sha256_hex(text),
            status: PlanStatus::Proposed,
            executed_by: None,
        };
        self.save(&plan)?;
        Ok(plan)
    }

    pub fn save(&self, plan: &Plan) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.path(&plan.id).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(plan)?)?;
        std::fs::rename(tmp, path)
    }

    pub fn get(&self, id: &str) -> Result<Plan, String> {
        let path = self.path(id)?;
        let bytes = std::fs::read(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => format!("no plan `{id}`"),
            _ => format!("{}: {e}", path.display()),
        })?;
        let plan: Plan =
            serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        if sha256_hex(&plan.text) != plan.sha256 {
            return Err(format!(
                "plan `{id}` was changed after it was proposed (its text doesn't match its sha256)"
            ));
        }
        Ok(plan)
    }

    /// Every plan, newest first.
    pub fn list(&self) -> Vec<Plan> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut plans: Vec<Plan> = entries
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| std::fs::read(e.path()).ok())
            .filter_map(|b| serde_json::from_slice(&b).ok())
            .collect();
        plans.sort_by(|a, b| b.created.cmp(&a.created));
        plans
    }

    /// Moves a proposed plan on; only a proposed plan can be decided.
    pub fn decide(&self, id: &str, status: PlanStatus) -> Result<Plan, String> {
        let mut plan = self.get(id)?;
        if plan.status != PlanStatus::Proposed {
            return Err(format!(
                "plan `{id}` is already {}",
                serde_json::to_value(plan.status)
                    .unwrap()
                    .as_str()
                    .unwrap_or("decided")
            ));
        }
        plan.status = status;
        self.save(&plan).map_err(|e| e.to_string())?;
        Ok(plan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plan_is_saved_hashed_and_decided_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = PlanStore::new(dir.path().join("plans"));
        let p = store
            .propose("tidy", Path::new("/w"), "s1", "1. read\n2. write")
            .unwrap();
        assert_eq!(p.sha256, sha256_hex("1. read\n2. write"));
        assert_eq!(p.sha256.len(), 64);
        assert_eq!(store.list().len(), 1);
        assert_eq!(
            store.decide(&p.id, PlanStatus::Approved).unwrap().status,
            PlanStatus::Approved
        );
        assert!(store
            .decide(&p.id, PlanStatus::Rejected)
            .unwrap_err()
            .contains("already approved"));
        assert!(execution_prompt(&store.get(&p.id).unwrap()).contains("2. write"));

        let mut tampered = store.get(&p.id).unwrap();
        tampered.text.push_str("\n3. rm -rf /");
        store.save(&tampered).unwrap();
        assert!(store.get(&p.id).unwrap_err().contains("changed"));
        assert!(store.get("../x").is_err());
    }
}

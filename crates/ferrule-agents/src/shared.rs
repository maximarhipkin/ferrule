//! The board and the task list as the agents see them: who may post, read
//! and claim what, and how it's fenced on the way to a model.

use crate::board::{ClaimRefused, Entry, Task, TaskStatus, MAX_POST_CHARS};
use crate::error::AgentsError;
use crate::fence::{cap, fence};
use crate::store::Status;
use crate::supervisor::{now, InboxItem, Supervisor};
use std::collections::HashMap;

/// Entries one `board_read` returns.
const READ_LIMIT: usize = 50;

impl Supervisor {
    /// Posts to `caller`'s board, or sends a direct message to `to`.
    pub fn post(
        &self,
        caller: &str,
        body: &str,
        topic: Option<&str>,
        to: Option<&str>,
    ) -> Result<String, AgentsError> {
        let me = self.get(caller)?;
        let n = body.chars().count();
        if body.trim().is_empty() {
            return Err(AgentsError::Invalid("the post is empty".into()));
        }
        if n > MAX_POST_CHARS {
            return Err(AgentsError::Invalid(format!(
                "a post is at most {MAX_POST_CHARS} characters, this one is {n}: put the details in a \
                 file and post its path"
            )));
        }
        if let Some(to) = to {
            let other = self
                .store()
                .get(to)?
                .filter(|r| r.tree == me.tree)
                .ok_or_else(|| {
                    AgentsError::Invalid(format!("there is no agent {to} in this tree"))
                })?;
            if other.id == me.id {
                return Err(AgentsError::Invalid("that's you".into()));
            }
            if other.status == Status::Closed {
                return Err(AgentsError::Invalid(format!("agent {to} is closed")));
            }
        }
        let id = self
            .store()
            .post(&me.tree, caller, to, topic, body, now())?;
        let Some(to) = to else {
            return Ok(format!("Posted entry {id} to the board."));
        };
        let entry = Entry {
            id,
            tree: me.tree.clone(),
            author: caller.to_string(),
            recipient: Some(to.to_string()),
            topic: topic.map(str::to_string),
            body: body.to_string(),
            created_at: now(),
        };
        // A message never wakes anyone: it waits for the recipient's next
        // step or next run.
        self.inbox(to).push(InboxItem {
            text: self.entry_fence(&entry, &mut HashMap::new()),
            about: None,
            wakes: false,
        });
        Ok(format!(
            "Sent message {id} to {to}. It reaches that agent at its next step, or when it next runs."
        ))
    }

    /// Board entries after `since` that `caller` may see, fenced.
    pub fn read(
        &self,
        caller: &str,
        since: Option<i64>,
        topic: Option<&str>,
    ) -> Result<String, AgentsError> {
        let me = self.get(caller)?;
        let since = since.unwrap_or(0);
        let entries = self
            .store()
            .read_board(&me.tree, caller, since, topic, READ_LIMIT)?;
        if entries.is_empty() {
            return Ok(if since > 0 {
                format!("Nothing on the board after entry {since}.")
            } else {
                "The board is empty.".into()
            });
        }
        let mut names = HashMap::new();
        let mut out: Vec<String> = entries
            .iter()
            .map(|e| self.entry_fence(e, &mut names))
            .collect();
        if entries.len() == READ_LIMIT {
            let last = entries.last().map(|e| e.id).unwrap_or(since);
            out.push(format!("There may be more: read again with since={last}."));
        }
        Ok(out.join("\n\n"))
    }

    fn name_of(&self, id: &str, names: &mut HashMap<String, Option<String>>) -> Option<String> {
        names
            .entry(id.to_string())
            .or_insert_with(|| self.store().get(id).ok().flatten().and_then(|r| r.name))
            .clone()
    }

    fn entry_fence(&self, e: &Entry, names: &mut HashMap<String, Option<String>>) -> String {
        let name = self.name_of(&e.author, names);
        let id = e.id.to_string();
        fence(
            "board_entry",
            &[
                ("id", Some(&id)),
                ("author", Some(&e.author)),
                ("name", name.as_deref()),
                ("origin", Some("agent")),
                ("untrusted", Some("true")),
                ("topic", e.topic.as_deref()),
                ("to", e.recipient.as_deref()),
            ],
            &e.body,
        )
    }

    pub fn task_add(
        &self,
        caller: &str,
        title: &str,
        detail: Option<&str>,
        after: &[i64],
    ) -> Result<String, AgentsError> {
        let me = self.get(caller)?;
        if title.trim().is_empty() {
            return Err(AgentsError::Invalid("the task needs a title".into()));
        }
        let text = format!("{title}{}", detail.unwrap_or(""));
        if text.chars().count() > MAX_POST_CHARS {
            return Err(AgentsError::Invalid(format!(
                "a task is at most {MAX_POST_CHARS} characters: put the details in a file and give its path"
            )));
        }
        let id = self
            .store()
            .add_task(&me.tree, caller, title, detail, after, now())?;
        Ok(if after.is_empty() {
            format!("Added task {id}.")
        } else {
            format!("Added task {id}, claimable once {} are done.", ids(after))
        })
    }

    pub fn task_list(&self, caller: &str) -> Result<String, AgentsError> {
        let me = self.get(caller)?;
        let tasks = self.store().tasks(&me.tree)?;
        if tasks.is_empty() {
            return Ok("There are no tasks.".into());
        }
        Ok(tasks
            .iter()
            .map(task_fence)
            .collect::<Vec<_>>()
            .join("\n\n"))
    }

    /// `caller` and everyone above it: whose tasks it may claim.
    fn lineage(&self, caller: &str) -> Result<Vec<String>, AgentsError> {
        let mut out = vec![caller.to_string()];
        let mut at = self.get(caller)?;
        while let Some(parent) = at.parent.clone() {
            out.push(parent.clone());
            at = self.get(&parent)?;
        }
        Ok(out)
    }

    pub fn task_claim(&self, caller: &str, id: Option<i64>) -> Result<String, AgentsError> {
        let me = self.get(caller)?;
        let lineage = self.lineage(caller)?;
        match self.store().claim(&me.tree, caller, &lineage, id, now())? {
            Ok(task) => Ok(format!(
                "You hold task {}. Do it, then call task_done with its id and your result.\n\n{}",
                task.id,
                task_fence(&task)
            )),
            Err(None) => {
                let waiting = self
                    .store()
                    .tasks(&me.tree)?
                    .iter()
                    .filter(|t| t.status == TaskStatus::Open && lineage.contains(&t.author))
                    .count();
                Ok(if waiting > 0 {
                    format!("No task is ready: {waiting} open ones wait for others to be done.")
                } else {
                    "No open task is left for you.".into()
                })
            }
            Err(Some(why)) => {
                let id = id.unwrap_or_default();
                Err(AgentsError::Invalid(match why {
                    ClaimRefused::NotFound => format!("there is no task {id} in this tree"),
                    ClaimRefused::NotOpen(status, by) => match by {
                        Some(by) => format!("task {id} is {} by {by}", status.as_str()),
                        None => format!("task {id} is {}", status.as_str()),
                    },
                    ClaimRefused::Blocked(on) => {
                        format!("task {id} waits for {} to be done", ids(&on))
                    }
                    ClaimRefused::NotYours(author) => format!(
                        "task {id} was added by {author}; you can only take tasks added by you or the \
                         agents above you"
                    ),
                }))
            }
        }
    }

    pub fn task_done(
        &self,
        caller: &str,
        id: i64,
        result: &str,
        failed: bool,
    ) -> Result<String, AgentsError> {
        let result = cap(result, MAX_POST_CHARS);
        if !self
            .store()
            .finish_task(id, caller, &result, failed, now())?
        {
            return Err(AgentsError::Invalid(format!(
                "you don't hold task {id}; task_claim it first"
            )));
        }
        Ok(if failed {
            format!("Task {id} is marked failed; tasks after it stay blocked.")
        } else {
            format!("Task {id} is done.")
        })
    }
}

fn ids(ids: &[i64]) -> String {
    let list: Vec<String> = ids.iter().map(|i| format!("task {i}")).collect();
    list.join(", ")
}

/// A task as a model sees it. The title and detail come from the claimer's
/// own line of agents; the result from whoever did it, so it's fenced
/// separately and marked untrusted.
fn task_fence(t: &Task) -> String {
    let id = t.id.to_string();
    let after = (!t.after.is_empty()).then(|| {
        t.after
            .iter()
            .map(|a| a.to_string())
            .collect::<Vec<_>>()
            .join(",")
    });
    let body = match &t.detail {
        Some(d) if !d.trim().is_empty() => format!("{}\n{d}", t.title),
        _ => t.title.clone(),
    };
    let mut out = fence(
        "task",
        &[
            ("id", Some(&id)),
            ("author", Some(&t.author)),
            ("status", Some(t.status.as_str())),
            ("claimed_by", t.claimed_by.as_deref()),
            ("after", after.as_deref()),
        ],
        &body,
    );
    if let Some(result) = &t.result {
        out.push('\n');
        out.push_str(&fence(
            "task_result",
            &[
                ("task", Some(&id)),
                ("by", t.claimed_by.as_deref()),
                ("untrusted", Some("true")),
            ],
            result,
        ));
    }
    out
}

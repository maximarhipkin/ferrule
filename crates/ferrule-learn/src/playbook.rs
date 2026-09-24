//! The playbook: a Markdown file of one-line lessons appended to every
//! agent's system prompt. Lines starting with `- ` are lessons; `[pb-N]`
//! marks the ones the learning pass wrote, and only those can it edit or
//! retire, one line at a time (ACE's delta updates, never a rewrite).
//! Everything else in the file is the owner's and is kept byte for byte.

use serde::{Deserialize, Serialize};

/// A new playbook, written the first time a pass runs.
pub const TEMPLATE: &str = "# Ferrule playbook

Lessons ferrule learned from failed runs. Edit freely: lines starting with
\"- \" are lessons; everything else is ignored. The learning pass only ever
changes lessons tagged [pb-N], one line at a time.
";

/// The first line of the `[Playbook]` prompt section.
pub const PROMPT_INTRO: &str = "[Playbook] Lessons from earlier runs on this machine. \
Follow them unless the task says otherwise.";

#[derive(Debug, Clone, PartialEq)]
enum Line {
    Lesson {
        id: Option<u32>,
        text: String,
        /// The line as read; `None` once the pass changed it.
        raw: Option<String>,
    },
    Other(String),
}

impl Line {
    fn render(&self) -> String {
        match self {
            Line::Lesson { raw: Some(r), .. } => r.clone(),
            Line::Lesson {
                id: Some(id), text, ..
            } => format!("- [pb-{id}] {text}"),
            Line::Lesson { id: None, text, .. } => format!("- {text}"),
            Line::Other(s) => s.clone(),
        }
    }
}

/// One lesson as prompts and the reflector see it.
#[derive(Debug, Clone, PartialEq)]
pub struct Lesson {
    /// `None` for the owner's own lessons, which the pass never changes.
    pub id: Option<u32>,
    pub text: String,
}

/// A change the reflector proposes.
#[derive(Debug, Clone, PartialEq)]
pub enum Delta {
    Add { text: String },
    Edit { id: u32, text: String },
    Retire { id: u32 },
}

impl Delta {
    pub fn op(&self) -> &'static str {
        match self {
            Delta::Add { .. } => "add",
            Delta::Edit { .. } => "edit",
            Delta::Retire { .. } => "retire",
        }
    }

    pub fn text(&self) -> Option<&str> {
        match self {
            Delta::Add { text } | Delta::Edit { text, .. } => Some(text),
            Delta::Retire { .. } => None,
        }
    }
}

/// A delta as applied: enough to undo it line by line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Applied {
    pub op: String,
    pub id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new: Option<String>,
    /// Index of the line in the file when it was applied.
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Playbook {
    lines: Vec<Line>,
    trailing_newline: bool,
}

fn parse_line(line: &str) -> Line {
    let Some(rest) = line.strip_prefix("- ") else {
        return Line::Other(line.to_string());
    };
    let rest = rest.trim();
    if rest.is_empty() {
        return Line::Other(line.to_string());
    }
    let (id, text) = match rest
        .strip_prefix("[pb-")
        .and_then(|r| r.split_once(']'))
        .and_then(|(n, t)| n.parse::<u32>().ok().map(|n| (n, t.trim())))
    {
        Some((n, t)) if !t.is_empty() => (Some(n), t.to_string()),
        _ => (None, rest.to_string()),
    };
    Line::Lesson {
        id,
        text,
        raw: Some(line.to_string()),
    }
}

impl Playbook {
    pub fn parse(text: &str) -> Self {
        let trailing_newline = text.ends_with('\n');
        let body = text.strip_suffix('\n').unwrap_or(text);
        let lines = if text.is_empty() {
            Vec::new()
        } else {
            body.split('\n').map(parse_line).collect()
        };
        Self {
            lines,
            trailing_newline,
        }
    }

    /// The file's text; byte-identical to what was parsed until a delta is
    /// applied, and then only the changed line differs.
    pub fn render(&self) -> String {
        let mut out = self
            .lines
            .iter()
            .map(Line::render)
            .collect::<Vec<_>>()
            .join("\n");
        if self.trailing_newline {
            out.push('\n');
        }
        out
    }

    pub fn lessons(&self) -> Vec<Lesson> {
        self.lines
            .iter()
            .filter_map(|l| match l {
                Line::Lesson { id, text, .. } => Some(Lesson {
                    id: *id,
                    text: text.clone(),
                }),
                Line::Other(_) => None,
            })
            .collect()
    }

    pub fn get(&self, id: u32) -> Option<&str> {
        self.lines.iter().find_map(|l| match l {
            Line::Lesson {
                id: Some(i), text, ..
            } if *i == id => Some(text.as_str()),
            _ => None,
        })
    }

    /// The largest `[pb-N]` in the file (0 when there is none).
    pub fn max_id(&self) -> u32 {
        self.lessons()
            .iter()
            .filter_map(|l| l.id)
            .max()
            .unwrap_or(0)
    }

    fn index_of(&self, id: u32) -> Option<usize> {
        self.lines
            .iter()
            .position(|l| matches!(l, Line::Lesson { id: Some(i), .. } if *i == id))
    }

    /// Applies `delta`; an add gets `new_id`. Only `[pb-N]` lessons can be
    /// edited or retired.
    pub fn apply(&mut self, delta: &Delta, new_id: u32) -> Result<Applied, String> {
        match delta {
            Delta::Add { text } => {
                let at = match self
                    .lines
                    .iter()
                    .rposition(|l| matches!(l, Line::Lesson { .. }))
                {
                    Some(i) => i + 1,
                    None => {
                        // The file's first lesson goes after a blank line.
                        let blank_end = match self.lines.last() {
                            None => true,
                            Some(Line::Other(s)) => s.trim().is_empty(),
                            Some(_) => false,
                        };
                        if !blank_end {
                            self.lines.push(Line::Other(String::new()));
                        }
                        self.lines.len()
                    }
                };
                self.lines.insert(
                    at,
                    Line::Lesson {
                        id: Some(new_id),
                        text: text.clone(),
                        raw: None,
                    },
                );
                self.trailing_newline = true;
                Ok(Applied {
                    op: "add".into(),
                    id: new_id,
                    old: None,
                    new: Some(text.clone()),
                    line: at,
                })
            }
            Delta::Edit { id, text } => {
                let i = self
                    .index_of(*id)
                    .ok_or_else(|| format!("there is no lesson pb-{id}"))?;
                let old = self.get(*id).unwrap_or_default().to_string();
                self.lines[i] = Line::Lesson {
                    id: Some(*id),
                    text: text.clone(),
                    raw: None,
                };
                Ok(Applied {
                    op: "edit".into(),
                    id: *id,
                    old: Some(old),
                    new: Some(text.clone()),
                    line: i,
                })
            }
            Delta::Retire { id } => {
                let i = self
                    .index_of(*id)
                    .ok_or_else(|| format!("there is no lesson pb-{id}"))?;
                let old = self.get(*id).unwrap_or_default().to_string();
                self.lines.remove(i);
                Ok(Applied {
                    op: "retire".into(),
                    id: *id,
                    old: Some(old),
                    new: None,
                    line: i,
                })
            }
        }
    }

    /// Undoes `applied` if the line is still as the delta left it.
    pub fn invert(&mut self, applied: &Applied) -> Result<(), String> {
        let id = applied.id;
        match applied.op.as_str() {
            "add" => match (self.index_of(id), self.get(id)) {
                (Some(i), Some(t)) if Some(t) == applied.new.as_deref() => {
                    self.lines.remove(i);
                    Ok(())
                }
                (None, _) => Err(format!("pb-{id} is already gone")),
                _ => Err(format!("pb-{id} was changed since; left as it is")),
            },
            "edit" => match (self.index_of(id), self.get(id)) {
                (Some(i), Some(t)) if Some(t) == applied.new.as_deref() => {
                    self.lines[i] = Line::Lesson {
                        id: Some(id),
                        text: applied.old.clone().unwrap_or_default(),
                        raw: None,
                    };
                    Ok(())
                }
                (None, _) => Err(format!("pb-{id} is gone; its old text was not restored")),
                _ => Err(format!("pb-{id} was changed since; left as it is")),
            },
            "retire" => {
                if self.index_of(id).is_some() {
                    return Err(format!("pb-{id} is back already"));
                }
                let at = applied.line.min(self.lines.len());
                self.lines.insert(
                    at,
                    Line::Lesson {
                        id: Some(id),
                        text: applied.old.clone().unwrap_or_default(),
                        raw: None,
                    },
                );
                self.trailing_newline = true;
                Ok(())
            }
            other => Err(format!("unknown change `{other}`")),
        }
    }
}

/// What prompts get: the `[Playbook]` section, and how many lessons were
/// left out by the caps.
pub struct PromptBlock {
    pub text: Option<String>,
    pub omitted: usize,
}

/// The `[Playbook]` section for a system prompt: at most `max_lessons`
/// lessons and `max_chars` characters, in file order, ids stripped. `None`
/// when the file has no lessons.
pub fn prompt_block(playbook: &str, max_lessons: usize, max_chars: usize) -> PromptBlock {
    let lessons = Playbook::parse(playbook).lessons();
    let mut out = String::from(PROMPT_INTRO);
    let mut used = 0;
    let mut kept = 0;
    for l in &lessons {
        let line = format!("\n- {}", l.text);
        if kept >= max_lessons || used + line.len() > max_chars {
            break;
        }
        used += line.len();
        kept += 1;
        out.push_str(&line);
    }
    PromptBlock {
        text: (kept > 0).then_some(out),
        omitted: lessons.len() - kept,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNED: &str = "# Ferrule playbook\n\nIntro prose.\n\n-  Always answer in Hebrew.\n- [pb-1] Run `cargo fmt --all` before finishing.\n- [pb-4] Reports go to reports/.\n\n## notes\n-\n";

    #[test]
    fn parse_and_render_round_trip_byte_for_byte() {
        for text in [OWNED, "", "no newline at end", TEMPLATE, "- a\r\n- b"] {
            assert_eq!(Playbook::parse(text).render(), text, "{text:?}");
        }
        let p = Playbook::parse(OWNED);
        let lessons = p.lessons();
        assert_eq!(lessons.len(), 3);
        assert_eq!(lessons[0].id, None);
        assert_eq!(lessons[0].text, "Always answer in Hebrew.");
        assert_eq!(p.get(4), Some("Reports go to reports/."));
        assert_eq!(p.max_id(), 4);
    }

    #[test]
    fn deltas_change_one_line_and_invert() {
        let mut p = Playbook::parse(OWNED);
        let add = p
            .apply(
                &Delta::Add {
                    text: "Check the lock file first.".into(),
                },
                5,
            )
            .unwrap();
        let text = p.render();
        assert!(
            text.contains(
                "- [pb-4] Reports go to reports/.\n- [pb-5] Check the lock file first.\n\n## notes"
            ),
            "{text}"
        );
        let edit = p
            .apply(
                &Delta::Edit {
                    id: 1,
                    text: "Run `cargo fmt --all` and clippy first.".into(),
                },
                0,
            )
            .unwrap();
        assert_eq!(
            edit.old.as_deref(),
            Some("Run `cargo fmt --all` before finishing.")
        );
        let retire = p.apply(&Delta::Retire { id: 4 }, 0).unwrap();
        assert!(!p.render().contains("reports/"));
        assert!(p.apply(&Delta::Retire { id: 9 }, 0).is_err());

        p.invert(&retire).unwrap();
        p.invert(&edit).unwrap();
        p.invert(&add).unwrap();
        assert_eq!(
            p.render().replace("-  Always", "- Always"),
            OWNED.replace("-  Always", "- Always")
        );
        assert_eq!(p.lessons(), Playbook::parse(OWNED).lessons());
    }

    #[test]
    fn invert_refuses_a_line_changed_since() {
        let mut p = Playbook::parse(OWNED);
        let add = p
            .apply(
                &Delta::Add {
                    text: "one lesson here".into(),
                },
                5,
            )
            .unwrap();
        p.apply(
            &Delta::Edit {
                id: 5,
                text: "owner rewrote it".into(),
            },
            0,
        )
        .unwrap();
        assert!(p.invert(&add).is_err());
        assert!(p.render().contains("owner rewrote it"));
    }

    #[test]
    fn the_first_lesson_goes_after_the_template() {
        let mut p = Playbook::parse(TEMPLATE);
        p.apply(
            &Delta::Add {
                text: "first lesson text".into(),
            },
            1,
        )
        .unwrap();
        let out = p.render();
        assert!(out.starts_with(TEMPLATE), "{out}");
        assert!(
            out.ends_with("one line at a time.\n\n- [pb-1] first lesson text\n"),
            "{out:?}"
        );
        p.apply(
            &Delta::Add {
                text: "second lesson text".into(),
            },
            2,
        )
        .unwrap();
        assert!(p
            .render()
            .ends_with("- [pb-1] first lesson text\n- [pb-2] second lesson text\n"));
        let mut empty = Playbook::parse("");
        empty
            .apply(
                &Delta::Add {
                    text: "only lesson".into(),
                },
                1,
            )
            .unwrap();
        assert_eq!(empty.render(), "- [pb-1] only lesson\n");
    }

    #[test]
    fn prompt_block_strips_ids_and_caps() {
        let b = prompt_block(OWNED, 40, 4000);
        let text = b.text.unwrap();
        assert!(text.starts_with(PROMPT_INTRO));
        assert!(text.ends_with("\n- Always answer in Hebrew.\n- Run `cargo fmt --all` before finishing.\n- Reports go to reports/."), "{text}");
        assert!(!text.contains("pb-"));
        assert_eq!(b.omitted, 0);
        let b = prompt_block(OWNED, 2, 4000);
        assert_eq!(b.omitted, 1);
        let b = prompt_block(OWNED, 40, 30);
        assert_eq!(b.omitted, 2);
        assert!(prompt_block(TEMPLATE, 40, 4000).text.is_none());
    }
}

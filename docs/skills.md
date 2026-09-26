# Skills

A skill is a folder with a `SKILL.md`: YAML frontmatter (a `name` and a
`description`), then instructions in Markdown, plus any files it bundles
(references, scripts). Ferrule reads the same format as Claude Code and
other Agent Skills clients, so existing skills work unchanged.

The model sees each skill's name and description in its catalog. When a
task needs one, it calls `activate_skill` and gets the instructions, and
`read_skill_file` for anything bundled. Since M28 a skill can also load
by itself when your message names one of its keywords (see
[Triggers](#triggers)).

## Where skills come from

Searched in this order; the first skill with a given name wins:

1. the workspace: `.ferrule/skills`, `.agents/skills`, `.claude/skills`
   (**project** skills, only with `[skills] project = true`, the default);
2. `[skills] paths`;
3. `~/.config/ferrule/skills`, `~/.agents/skills`, `~/.claude/skills`
   (**user** skills);
4. skills the agent installed itself with `skill_install`, under
   `<data>/extensions/skills`. They are scanned before install and
   locked to the digest of their `SKILL.md` (docs/m13-self-extension.md).

`ferrule skills` lists every skill found, its scope and any warnings.
`ferrule skills disable <name>` / `enable <name>`, or `/skills` in
Telegram, turn one off and on.

For a tool rather than instructions, a WASM plugin (`docs/plugins.md`)
adds one in a capability sandbox.

## Triggers

Add `triggers:` to the frontmatter, and the skill loads with any message
of yours that names one of them. You don't have to wait for the model to
decide it needs it.

```yaml
---
name: release
description: How we cut a release.
triggers: [ship it, release, "cut a release", שחרור]
---
```

The list can be a flow list (as above), a block list of `- item` lines,
or a comma-separated string. Each entry is a word or a phrase.

- At least 2 letters or digits each; at most 20 per skill. Extra and
  too-short ones are dropped, with a warning in `ferrule skills`.
- **No regex, no wildcards, no stemming.** List the forms you want
  (`release`, `releases`).

### How a message matches

- **Whole words, any case.** `ship` matches "Ship it!" but not
  "shipping" or "reship".
- **Phrases are consecutive words.** `ship it` matches "SHIP, it"
  (punctuation separates words) but not "ship this".
- **Accents and niqqud don't matter.** `cafe` matches "Café", and
  `שָׁלוֹם` matches `שלום`. An apostrophe or geresh between letters stays
  inside the word (`don't`, `צ׳יפס`, `צה״ל`).
- **Hebrew prefixes.** A Hebrew trigger word also matches with 1 to 4 of
  the letters ו ה ב ל מ ש כ in front of it: `שחרור` matches `השחרור`,
  `ושחרור`, `לשחרור`, `וכשהשחרור`, but not `שחרורים` or `אשחרור`. This
  works for every word of a phrase (`הבדיקה המהירה`). Write triggers
  **without** a prefix: `השחרור` won't match a bare `שחרור`.

### What loads, and when

- **Only what you type.** Your `ferrule run` prompt, each line of
  `ferrule chat`, and each message from a chat channel. Nothing else
  is matched: a web page, a search result, a tool's or MCP server's
  output, a sub-agent's report, a scheduled task's prompt, an approved
  plan being carried out. That text can't pull a skill into context.
  (A scheduled prompt can still ask for a skill by name.)
- **At most 2 per message**, earliest match first, within about 4000
  tokens together. A skill that matched but doesn't fit isn't loaded;
  the model is told it matched and can activate it itself.
- **Once.** A skill already in the conversation isn't loaded again. If
  compaction dropped it, the next message that names it loads it again.
- **Only vetted skills.** At every match, before loading, the skill's
  files go through the same scan as `skill_install`, and a blocking
  finding stops it. A skill the agent installed must also still be
  active in the extensions lock with its `SKILL.md` unchanged: one
  edited after install, or suspended, never loads by trigger.
- **Not hidden ones.** A skill with `disable-model-invocation: true`
  never loads by trigger.
- **Project skills don't trigger** unless you set
  `project_triggers = true`. A repo you cloned can put skills in its
  `.claude/skills`, and those shouldn't load themselves on common words
  unless you say so.
- Only for the agent you talk to, not its sub-agents.

The skill goes into the conversation right after your message, as the
same block `activate_skill` returns, never into the system prompt. So a
trigger doesn't break the provider's prompt cache (docs/speed.md).

### Seeing it happen

- `ferrule run` and `ferrule chat` print `[skill `release` loaded:
  "ship it"]`.
- The loaded text starts with a note the model and the transcript both
  see: `[ferrule: the skill `release` was loaded because the message
  says "ship it"]`.
- The transcript records `skill_triggered`, and `skill_trigger_skipped`
  (over the budget) or `skill_trigger_refused` (failed vetting, with the
  reason). The refusal also goes to the log at `warn`.
- `ferrule skills` and Telegram `/skills` show each skill's triggers,
  and say when they're off and why.

## Config

```toml
[skills]
enabled = true
project = true                 # the workspace's skills are in the catalog
paths = []
disabled = []                  # skill names to ignore
triggers = true                # load a skill when your message names its triggers
max_triggered = 2              # skills one message loads that way
trigger_budget_tokens = 4000   # their combined size (estimated at 4 chars a token)
project_triggers = false       # let the workspace's skills trigger too
```

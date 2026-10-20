# M57: context and loop hygiene

What the coding agents that lead the field do to keep a long session's
context small and its loop honest, built into ferrule's loop. The items
come from the 2026-10-20 gap analysis (kept outside the repo; its M57
section is summarized in `docs/roadmap.md`'s "Other open tracks"). Each
section says what changed, where, and what is left open.

## Deferred tools and `tool_search` (M57.1)

Every request carries every tool's schema. An owner with a few MCP
servers sends thousands of tokens of schemas with each message, most of
them for tools that turn will never call. Anthropic measured definitions
falling from about 77K to 8.7K tokens with deferred loading, and MCP
tool choice getting more accurate
([advanced tool use](https://www.anthropic.com/engineering/advanced-tool-use)).

**What the model sees.** A *deferrable* tool goes out as its name and the
first line of its description, in a list inside one extra tool,
`tool_search`. The model calls `tool_search {query}` with words for what
it needs (`"create github issue"`) or `select:<name>,<name>` for exact
names. The matching tools' full schemas come back as the result (five at
most by default, `max_results` up to 20), and from the next request on
those tools are offered whole, like any other. A deferred tool called by
name without a search runs anyway and is loaded the same way, so a
model that guesses right loses nothing.

**Which tools are deferrable** (`Tool::deferrable`, in
`crates/ferrule-core/src/tool.rs`): every MCP tool (`mcp__<server>__*`,
the browser's and connections' included), the self-extension tools
(`mcp_add`, `skill_install` and the rest), `connection_request`, and
`generate_image` and `generate_video`. Everything else (the file tools,
the shell, memory, sub-agents, the chat tools) is always sent whole.

**When** (`[agent] defer_tools`):

```toml
[agent]
defer_tools = "auto"    # the default: once more than 10 tools are deferrable
# defer_tools = "always" # whenever any is
# defer_tools = "off"    # every schema with every request, as before
```

`auto` decides per session and doesn't change its mind: once a session
has had more than 10 deferrable tools it keeps deferring, even if a
server goes away. With nothing deferrable there is no `tool_search`.

**The cache.** The tool list is the first thing in a request, so it has
to stay the same bytes for the prompt cache to hit
([speed.md](speed.md#the-prompt-cache-hits)). Deferring keeps it that
way: the tools that aren't deferred and `tool_search` come first, sorted
by name, and `tool_search`'s list names every deferred tool, loaded or
not, so it doesn't change when one is loaded. Each loaded tool's schema
is appended after them, in the order it was loaded. A load therefore
costs one cache miss (everything after the tool list is read again at
the write price once), never a rewrite of the list. What does rewrite it
is what rewrote it before: an MCP server added or removed, which changes
`tool_search`'s list too, and `auto` turning on mid-session.

**Sessions.** Loads live in the agent's tool registry, so they last as
long as the session's agent: a chat's lane keeps them across turns, a
compaction doesn't touch them. A session resumed from its transcript
loads again every deferred tool its history called; one it only searched
for comes back with another search, or when it's called.

**The Claude plan.** Claude Code has its own tool search over MCP tools,
so the engine (`crates/ferrule-plans/src/claude/engine.rs`) bridges every
ferrule tool whole, deferred ones included (the agent hands them over in
the call context, `CallContext::deferred_tools`), and leaves
`tool_search` out.

**Open.** Nothing was measured live: the token savings are Anthropic's
numbers, not ferrule's. The ledger's input and cache rows
(`ferrule ledger`) show the effect on a real session. The threshold of
10 is a guess to tune. Search is word matching on names and
descriptions, not embeddings.

## Loops per file, a visible budget, and raised effort (M57.2)

LangChain's harness work gained 13.7 points from a bundle of loop
changes: loop detection per file, forced verification and budget
warnings. Its "reasoning sandwich" (more effort for planning and
verifying, less in between) scored 66.5% against 63.6% for high effort on
every turn and 53.9% for the highest
([LangChain](https://www.langchain.com/blog/improving-deep-agents-with-harness-engineering)).
Ferrule already forces verification (the M9 check); these are the other
two, and its version of the sandwich.

**Edits per file** (`EditCounts` in `crates/ferrule-core/src/stuck.rs`).
The stuck detector catches the same call repeated with the same result.
A model fixing one file over and over, with a different edit each time,
slipped past it. Now every successful `edit_file` or `write_file` counts
against its path, and the sixth change to one file with no passing check
in between gets one note after that turn's results: re-read the file and
the error, and check the approach before editing again. A passing check
starts every count again. It's on with the stuck detector
(`detect_stuck`) and never stops a run by itself.

**The budget, said.** When a run is down to a quarter of its steps
(`max_iterations`, 60 by default), and again at a tenth, a short note
after that turn's tool results says how many are left. A sub-agent also
hears its tree's token budget (`[agents] max_tokens`, through
`Budget::left`) at the same two points. The note goes at the end of the
history like any message, never into the system prompt, so the cached
prefix doesn't change. The owner's spend caps (M19) aren't reported this
way; they stop the run with a status as before.

**Raised effort.** A run's first turn, where the plan is made, and the
turn after a failed check ask for the profile's `max_effort` ("high" in
every built-in profile) as a floor. This only happens where the owner set
an `effort` for the model (`effort = "medium"` on its provider or model,
[models.md](models.md)): a model with none set may not accept the field,
so nothing is sent. A configured effort above "high" stays, and
`effort = "none"` (reasoning off) is left alone. The drivers that send an
effort are Anthropic's (`output_config.effort`) and the Responses and
Codex ones (`reasoning.effort`); the chat driver sends none.

```toml
[agent]
effort_boost = true    # false: every turn at the configured effort
```

**`spawn_agent {effort}`.** A parent can run a child at `low`, `medium`
or `high` for all of its turns: low for lookups and mechanical edits,
high for hard debugging. It's held to the child's profile's `max_effort`,
applies only where the child's model has an effort set (as above), and
replaces the raised turns: an explicit level wins. The level is kept with
the child (the `effort` column in `agents.db`), so a resumed child keeps
it.

**Open.** Not measured on ferrule's own suite (`ferrule eval`). Whether
changing `output_config.effort` between turns costs Anthropic prompt-cache
hits isn't stated in Anthropic's docs; the ledger's cache rows on a run
with `effort` set would show it.

## A stale-file guard on `edit_file` (M57.3)

`edit_file` read the file and applied the SEARCH blocks to whatever was
there. If a command, a hook, a sub-agent in the same workspace or an MCP
tool had changed the file since the model read it, the edit was planned
against bytes that were gone. An exact SEARCH usually failed then, but
one that still matched could land in a file the model no longer knew.
Claude Code refuses such an edit and asks for a re-read
([tools reference](https://code.claude.com/docs/en/tools-reference)).

**The map** (`FileStamps`, `crates/ferrule-core/src/stamps.rs`). Each
agent has one, shared by its `read_file`, `write_file` and `edit_file`
(`.tracking(stamps)`, in `build_agent_from`). It keeps, per resolved
path, the length and an FNV-64 hash of the bytes the agent last read or
wrote there (and the file's modified time, which only saves a re-read
when nothing touched the file). A sub-agent has its own map, so what a
child changes is a change to its parent.

**The refusal** (`edit_path`, `crates/ferrule-tools/src/edit.rs`).
`edit_file` compares the bytes it just read with the stamp. When they
differ it writes nothing and says so: the file changed on disk since you
last read it; re-read it with `read_file`, then make the edit against
what it holds now. The refusal stands until the model reads the file
again (or replaces it with `write_file`): a refused or failed edit
doesn't count as a read. A successful edit stamps the bytes it wrote.

**No stamp, no guard.** A file the model never read with `read_file`
(it saw it through the shell, `code_search` or the repo map, or not at
all) stays editable as before: SEARCH has to match exactly either way.
A file `read_file` can't read as UTF-8 has no stamp either.

**What doesn't count as a change.** The agent's own hooks and checks
aren't someone else: what they change is taken as seen
(`FileStamps::adopting`, around `Agent::fire` and the checks in
`run_checks`). Before they run, the agent notes which stamped files
still match the disk; after, it re-stamps those with what is there now.
A file that was already stale stays stale. So:
- the built-in per-edit lint (M29) only checks (`rustfmt --check`,
  `ruff check`, `gofmt -l`, `eslint`, `tsc --noEmit`) and writes nothing,
  and an owner's PostToolUse hook that formats the file it follows is
  adopted;
- a verify command that formats (`cargo fmt && cargo test`) doesn't make
  the next edit refuse.

A command the model runs itself with `shell` (`sed -i`, `cargo fmt`,
`git checkout`) is a change: its view of the file is stale, so the next
edit asks for a re-read.

**Where it's on.** Every agent `build_agent_from` makes (`ferrule run`,
`ferrule chat`, the gateway's lanes, sub-agents), and the eval's
engineered variants (not the naive one). The remote file tools of an SSH
workspace (M34) don't track: their `edit_file` keeps its own guard,
which refuses a write if the file changed between its read and its
write, but not since the model's last `read_file`.

**Open.** A file changed by a sub-agent while the parent's hooks or
checks run is adopted with them. That window is the length of a hook or
a check run (a long test suite, say). Not measured on the eval suite.

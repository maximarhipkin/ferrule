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

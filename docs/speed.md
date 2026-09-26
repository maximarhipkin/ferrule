# Speed

Three things make an answer arrive sooner. All are on by default, and none
changes what the model is asked or what it answers. The design, with the
reasons for each decision, is `docs/m27-speed.md`.

## Reads run at the same time

When the model asks for several tool calls in one response, the ones that
only read (`read_file`, `list_dir`, `code_search`, `web_fetch`, `recall`,
`search_history`, MCP tools the server marks `readOnlyHint`) run together. Anything that
writes (`write_file`, `edit_file`, `shell`, spawning an agent, other MCP tools) runs on
its own, in order, so `read a, read b, write a, read a` still reads the old
`a` twice and the new `a` once. Approvals are still asked one at a time,
before anything runs. Results go back to the model in the order it asked.

```toml
[agent]
parallel_tools = 4   # at most this many at once; 1 = one after another
```

A stdio MCP server gets one call at a time, since many can't take more.

## Replies grow as they're written

On Telegram, a reply appears once there's a line of it (or after a second)
and grows in place about once a second, instead of arriving whole at the
end. A quick answer arrives exactly as before, as one message. A long one
continues in a new message before Telegram's size limit. When the model
first says "let me look…" and then calls a tool, that text is replaced by
the real answer. Telegram's rate limits are respected, and if an edit
fails, the answer is sent as a new message, so it's never lost.

`ferrule chat` prints the reply as it comes too.

```toml
[agent]
stream = true            # everywhere it's possible

[gateway]
telegram_stream = false  # but not on Telegram (unset: follow [agent] stream)
```

Discord and Slack stream the same way, with `discord_stream` and
`slack_stream` (M31). Discord rolls over at 2000 characters; Slack edits at
most every 1.5 s and rolls over at 3900. See [discord.md](discord.md) and
[slack.md](slack.md).

Scheduled tasks, `ferrule run`, `ferrule eval` and channels that can't edit
a sent message get the whole reply at the end, as before. All three model
APIs stream (`api = "chat"`, `"anthropic"`, `"responses"`). A
compatible server that ignores streaming still works.

## The prompt cache hits

Providers bill a repeated request prefix at a cheaper cached price and
answer it faster. Ferrule keeps the start of every request byte for byte
the same: the tools in a fixed order, then the system prompt, then the
history, which only grows. Memory recalled for a session goes in right
after your request, not into the system prompt, so the system prompt is
the same for every session and caches across them.

What legitimately starts a new cache:

- an MCP server's tool list changing (`tools/list_changed`, adding or
  suspending a server);
- installing or removing a skill;
- a learning pass that changes the playbook;
- switching model (`/model`, routing's escalation, a fallback);
- compaction, which rewrites the history.

## Seeing it

```bash
ferrule ledger --since 7d
```

Under the usual table:

```
cache hit: 63.2% of input tokens
speed: first token p50 420ms (n=31) · first reply p50 1.3s (n=12)
parallel batches: 9, 1.8s wall vs 5.1s summed
```

- **first token**: from sending a streamed request to its first byte.
- **first reply**: from the start of a turn to the first text there was to
  show. Telegram shows it at most a second later.
- **parallel batches**: the time those tool calls took together, against
  what they'd have taken one by one.

Each line shows only when there's data for it. The dashboard's usage
summary shows the cache hit too.

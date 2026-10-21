# M62 — compaction that can see the work

The summary that replaces the older half of a long session was being
asked to reconstruct the work from message text alone, and the request
that asked it had no size limit. A tool call went out as `assistant:`
followed by `tool:` and the result text: no tool name, no arguments.
The system prompt was in that text too, so every compaction spent
tokens summarizing it again. And when the folded history was large
enough, the summary call itself came back "prompt is too long", the
error ended the run, and the history was unchanged, so the next turn
failed the same way.

Code: `crates/ferrule-core/src/summary.rs`, called from `Agent::compact`
in `crates/ferrule-core/src/agent.rs`. The checklist the model fills is
`COMPACTION_TEMPLATE` in `crates/ferrule-core/src/profile.rs`.

## What the summary model is shown

The folded head is rendered before it is sent.

- System messages are left out. The system prompt stays in the history,
  ahead of the summary, as it did; it is not part of the text being
  summarized.
- An assistant tool call is `assistant → name(arguments)`. Long string
  arguments (a file's contents, a pasted body) are shortened first, and
  the arguments as a whole stop at 2,000 characters.
- A tool result is `tool name: …`, or `tool name (error): …` when the
  result is an error, a refusal, or a call that never ran. The gist is
  at most 4,000 characters. An error keeps its head and its tail, so
  the message at the end of a long failure is still there. A result
  replaced by a later identical one is not called an error.
- Skill blocks are still elided here and carried forward verbatim after
  the summary, as before.
- One message stops at 10,000 characters, the cap OpenHands puts on
  each event it feeds its condenser (`max_event_length`,
  [condenser_config.py](https://github.com/OpenHands/OpenHands/blob/26fa1185/openhands/core/config/condenser_config.py)).

The counts of tools and files ferrule appends after the summary
(`Touched`, M57.4) are unchanged. So is the rule that a fold never
cuts between a tool call and its results.

## The template

The model still fills a checklist, and it still starts "Summarize this
agent session". The sections are now the ones a later turn needs, the
same list Claude Code's compaction prompt uses
([an unofficial reconstruction](https://github.com/Piebald-AI/claude-code-system-prompts)):
every user request, in order and close to the original words; the
decisions; files touched, with full paths, and the other resources that
matter; errors quoted and how they were fixed, or that they are still
open; pending tasks; and the work in progress immediately before the
summary. "none" is still the right answer for an empty section. The
user's current request is still copied verbatim under the summary, as
before.

## Staying inside the window

The transcript piece is budgeted from the agent's context window: the
template, the output reserve and a tenth of the window (at most 8,192
tokens) are held back, because the byte/4 estimate runs low on some
texts. A window as small as the tests' 1,000 tokens still gets a piece
large enough for a short head.

When the rendered head is longer than that, it is summarized in pieces,
oldest first, split on message boundaries. Each later piece carries the
summary so far, shortened so the new piece still fits.

The budget is the agent's window, not a separate one for
`[agent] compaction_model`, which doesn't carry a window of its own. If
the summary model answers "prompt is too long" anyway, the same head is
summarized again in pieces half the size, up to three more times.

If the summary call still fails — too long, or any other error — the
fold happens anyway. The summary text is then a shortened extract of
the rendered head (its start and its end, at most 12,000 characters)
that says the summary model failed and that this is not a model
summary. The run continues, and the oversized tool result is no longer
in the live history, so the next turn is not the same request again.
A run that was only the system prompt folds a one-line note and does
not call the model.

## Open

Nothing here was run against a live model; the tests drive a stand-in
provider. The budget is the agent's window, so a much smaller
compaction model pays for one rejected call before the pieces shrink.
The extract keeps the shape of the rendered head, not the checklist
sections: a failed summary is marked as one, and the next compaction
can summarize it properly. Re-attaching the last few files' contents
after a fold, which Claude Code does with the five most recently
accessed files
([Anthropic, effective context engineering](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents)),
is not done here. The paths are already listed by `Touched`.

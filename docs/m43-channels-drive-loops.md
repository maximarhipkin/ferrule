# M43 — channels drive loops and graphs

**Status.** Built (2026-10-01).

M42 built goal loops and agent graphs at the terminal. M43 puts the
owner's chat in charge of both: a Telegram message starts a loop or a
graph, an approval node's question arrives as a chat approval, and the
ending is reported where it started.

## `/goal <what done looks like>` — a loop from a chat

- Owner-only (`trust::owner_in`); anyone else gets the refusal. `/goal`
  with no argument lists the open loops (goal, judge runs, last failing
  word, session id).
- The judge comes from `[agent] verify_command` (the same
  `goal::prepare` the CLI uses — a chat-started loop resumes from the
  terminal with `ferrule run --resume <session>` and vice versa, one
  state file). No judge configured → the reply says how to set one.
- The loop runs on its own router lane under the reserved pseudo-channel
  `goal` (never registered, so the lane's own reply path no-ops — the
  scheduler's M3 pattern). The chat's own lane is never held. The lane
  session id *is* the goal session id, so the agent factory picks the
  loop up from the `<sid>.goal.json` state file exactly like a CLI run:
  judge without file changes, verdicts recorded as they happen.
- Completion is delivered to the **originating** chat (not just the
  owner's private one) through the door's own channel handles: `✅ goal
  met` with the judge-run count, `⏸ goal pending` with the judge's last
  word and the resume command, `⚠️` on an error.

## `/graph <file> [--yes]` — a graph from a chat

- Owner-only; the path is relative to the gateway's workspace. `--yes`
  approves every gate.
- The graph runs on the gateway's **shared** supervisor: `graph::run`
  takes a `RunOpts` with an optional supervisor (a terminal run still
  builds and claims its own — one claim per process on the agents store).
  Children inherit the role models and the default model rule; `--model`
  is a terminal-only override.
- **Approval nodes ask the owner's chat.** `RunOpts.hub` routes them to
  the trust hub's `ask_owner` — Allow/Refuse buttons, the configured
  timeout, the audit event — instead of a terminal question. A refusal,
  a timeout or an unreachable owner fails the node closed and takes its
  `fail` edge.
- `RunOpts.quiet` collects the report (ANSI stripped) instead of printing
  live; the door sends the final `graph: goal met / not met` line with a
  ✅/⏸ when the run ends.

## Tests

`crates/ferrule-cli/tests/it/doors.rs`, against a fake Telegram and the
real gateway binary: a chat-started loop reports `Goal met` (state file
asserted: goal, one judge run, `goal__` session), a failing judge reports
`Goal pending` with the resume command, a chat-started graph reports
`graph: goal met`, and a stranger's `/goal` never starts anything. The
`--yes` graph path, the hub approval path's terminal/`--yes` equivalents
and the report formatter are covered by M42's graph tests and the trust
crate's approval tests.

## Not done

- Approvals for graphs running in a *terminal* process still ask at the
  terminal — `ask_owner` needs the notifier, and it lives in the gateway
  (cross-process approval relay is its own piece).
- No `/graph stop`; a chat-started graph can't be cut short from the chat
  (the kill switch covers it globally).
- The HTTP channel gets the doors too (it is a chat channel), but
  long-running loops deserve its webhook, not a held request.

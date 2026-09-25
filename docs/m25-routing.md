# M25: routing Phase 1 — start cheap, escalate when needed (design)

Status: design, 2026-09-25, branch `m25-routing` (cut from `main` after
M23). The user guide is `docs/routing.md`. Background: Phase 1 of
`docs/research-routing-and-local-models.md`; the model layer it sits on is
M21 (`docs/m21-models.md`) and M23 (`docs/m23-drivers.md`).

## Why

Most of what an agent does is routine: read a file, run a command, write a
reply. A cheap model does that well enough at a tenth of the price. The
hard parts are the ones where a cheap model visibly fails: a malformed tool
call, a check that keeps failing, going in circles. Phase 1 runs every turn
on the cheap tier and moves to a stronger one **only when one of those
failures shows up**. Nobody guesses up front how hard a request is.

## 1. The decisions

| # | Decision | Why |
|---|---|---|
| 1 | Escalate on **failure signals only** (§3). No model or heuristic judges difficulty up front. | A classifier call costs on every turn and is wrong in both directions; a failure is evidence. Brief (a). |
| 2 | Escalation is **sticky for the rest of the turn**: the level never goes down inside one `Agent::run`. | No ping-pong between tiers, and the prompt cache of the tier we moved to stays warm. |
| 3 | **One tier up per signal**, not straight to the top. | With two tiers it's the same thing. With three, a middle tier that copes is cheaper than the top one. A second signal on the middle tier moves up again. |
| 4 | **De-escalation at the next turn**, on by default (`de_escalate = true`). Off: the level holds for the session. | A chat's next message is usually a new, easier request. A scheduled task run is one turn, so "sticky for the task" is the same rule. |
| 5 | Routing is **off by default**. Off means the `Provider` trait's new methods are no-ops and the loop does no extra work: byte-identical requests, ledger rows and events (a golden test holds it). | Brief (b). |
| 6 | Tiers are **refs of connected models** (M21: alias, `provider/model`, …), ordered cheap → strong, two or more, on any drivers. | M21 already resolves and prices every connected model; M23 already carries a conversation across drivers. |
| 7 | **Tier refs** `tier:cheap`, `tier:strong`, `tier:<n>` (0-based), and the tier's own name if it has one, are accepted wherever a model word is: a chat pin, a task's model, a role's `model`, `spawn_agent(model)`. They set the **floor**: the tier the turn starts on. | One mechanism covers the owner hint "a task, sub-agent role or chat pinned to `tier = strong`". A floor, not a fixed model, so a `tier:1` pin on three tiers can still escalate to tier 2. |
| 8 | A **concrete model** (a pin, `--model`, a task's model or a role's model that isn't a tier ref) is not routed. | The owner asked for that model; routing would override them. |
| 9 | Escalation picks the **wanted** model; M21's fallback still swaps it for the first fallback that isn't down. Both apply, independently (§5). | A fallback is about an outage, an escalation about quality. |
| 10 | `/model strong` in a chat forces **the next turn** onto the top tier (once, then it's used up). `/model tiers` (or `/model`) shows the tiers. | Brief: an explicit owner hint. Once, because a sticky "strong" is what a pin is for (`/model tier:strong`). |
| 11 | An optional **daily cap on spend above the cheap tier**, `strong_daily_usd`. When today's spend on tiers above tier 0 reaches it, nothing escalates, and a tier floor above 0 runs on tier 0, both with a one-time note to the owner and an audit event. Concrete pins aren't limited by it. | Brief (a): escalations count against M19's caps anyway (§6); this is a tighter, routing-only cap. |
| 12 | Every call's ledger row carries `route: {tier, escalated?}`. `escalated` is the reason, on the first call after an escalation. Rows without routing have no `route` field at all. | Brief: "the ledger logs it with its reason". One optional field keeps old rows and readers unchanged. |
| 13 | **M16 sees escalations read-only**, through the ledger. It doesn't tune routing. | Brief: read-only is fine; a learning loop that changes thresholds is Phase 2, which was dropped. |
| 14 | Compaction runs **on the tier the turn is on** when it compacts. | Its summary call goes through the same provider, so no special case. M23's lossy cross-driver rules already cover a history that moves between drivers. |

## 2. Config

```toml
[routing]
enabled = true                       # default false: routing is off
tiers = ["cheap", "strong"]          # model refs, cheap → strong (2+)
de_escalate = true                   # back to the floor at the next turn
strong_daily_usd = 2.0               # optional: cap on spend above tier 0, per UTC day

# Optional: which signals escalate (all on by default).
[routing.triggers]
call_failed = true      # a call failed with a class retrying won't fix
tool_errors = 2         # this many invalid tool calls in a row (0 = off)
checks = true           # a verify check failed
stop_hooks = true       # a Stop hook sent the answer back
no_progress = 3         # the same tool with the same arguments this many times in a row (0 = off)
watchdog = true         # the gateway's watchdog saw the session stall
```

- `tiers` entries resolve through M21's catalog. A tier that doesn't
  resolve makes routing say so once (a warning and `doctor`) and route on
  the tiers that do; fewer than two means routing is off.
- `enabled = false` with `tiers` set: nothing is routed by default, but a
  tier ref (a pin `tier:strong`) still resolves to that tier's model, as a
  plain model with no escalation. That lets an owner use the names without
  turning routing on.
- Editing `[routing]` takes effect at the next call, like every M21 edit
  (the config's mtime is watched).

**Where the turn starts** (M21 precedence, with a tier ref allowed at every
level): fixed (`--model`, role, `spawn_agent`) → the task's model → the
chat's pin → the default. The default level is routed from tier 0 only when
`enabled`; otherwise it is `[models] default`, as today.

**Sub-agent roles** (M12): a role can say `model = "tier:strong"` (a planner
that starts strong) or `tier:cheap` (workers that start cheap). Each child
gets its own provider, so its tier state is its own. The shipped config
example is a strong planner and cheap workers; there is no built-in role
default, because without `[routing]` there are no tiers to name.

## 3. The signals

The agent loop reports a **signal** to its provider (`Provider::escalate`).
A provider that doesn't route (every one today) ignores it. The routing
provider decides: is the trigger on, is there a tier above, does the cap
allow it. If it moves up, the loop emits `AgentEvent::Escalated{from, to,
reason}`.

| Signal | Fires when | Reason in the ledger |
|---|---|---|
| `CallFailed(class)` | A call fails with an error retrying the same model won't fix: `Malformed`, `ContextTooLong`, `Refused`, `BadRequest`, or `ModelNotFound` whose text says the model doesn't support tools (OpenRouter's "No endpoints found that support tool use"). The **same request** is sent again at once on the new tier. | `call_failed:<class>` |
| `ToolErrors(n)` | The model called a tool that doesn't exist, or whose arguments aren't a JSON object or miss a field the tool's schema requires. `n` counts them in a row; any valid call resets it. Escalates at `tool_errors` (2). | `tool_errors` |
| `CheckFailed` | A verify check (`verify_command`, a `check = true` hook, eval's `CommandVerifier`) failed when the model said it was done. | `check_failed` |
| `StopHook` | An M18 Stop hook sent the answer back. | `stop_hook` |
| `NoProgress` | The same tool with the same arguments `no_progress` (3) times in a row, or the M9 stuck detector's first nudge. | `no_progress` |
| `Watchdog` | The gateway's M19b watchdog saw the session stall. The *next* call of that session escalates. | `watchdog` |
| owner hint | `/model strong`, consumed at the start of the next turn. | `owner` |

Transient errors (rate limits, 5xx, timeouts, connection) are **not**
signals: they are outages, which retries and M21's fallback handle (§5).
`Auth` isn't either: the strong model's key being fine says nothing about
the cheap one's.

A signal on the top tier does nothing. The loop's own handling (sending a
failed check back, the stuck nudge, wrap-up at the limits) is unchanged: the
signal is **in addition**, so routing only ever changes which model answers
next, never what it's told.

### Why these and not others

- *A long answer, a big diff, many tool calls*: size isn't failure.
- *A verifier sub-agent (M12 role) saying "doesn't hold"*: its verdict is
  free text, so there's nothing reliable to key on; the verify check and a
  Stop hook are structured. A follow-up could give the verifier role a
  verdict line.
- *A time-based "no progress"*: the watchdog already is one, for the
  gateway; within the loop, step repetition is the evidence.

## 4. How it's built

**Core (`ferrule-core/src/routing.rs`).**
- `Signal`, `Escalation{from, to, reason}`, `RouteTag{tier, escalated}`.
- `Policy`: `de_escalate`, the triggers and thresholds.
- `Ladder`: the per-agent state machine: tier names, floor, current level,
  the pending reason. `begin_turn(force)`, `escalate(signal, allow)`,
  `serve() -> (level, RouteTag)`.
- `Tiered`: a `Provider` over `Vec<(name, Arc<dyn Provider>, Served)>` with
  a `Ladder`, for eval and tests.
- `Provider` gets four default methods, no-ops unless overridden:
  `routes() -> bool`, `begin_turn()`, `escalate(&Signal) -> Option<Escalation>`
  and `route_tag() -> Option<RouteTag>`.
- The loop calls `begin_turn` at the top of `Agent::run`, `escalate` at the
  points in §3, and puts `route_tag()` on each ledger row. When `routes()`
  is false it doesn't even count tool errors or repeats.

**CLI (`models/routing.rs`).**
- The catalog reads `[routing]` and resolves tier refs. `Models::pick`
  returns the wanted entry *and* its tier ladder spec, when routed.
- `RoutedProvider` keeps a `Ladder` per agent. On each call it takes the
  tier the ladder is on, then runs M21's outage fallback on that entry.
- `Models` keeps: the one-shot `/model strong` hints per chat, the
  watchdog's stalled sessions, today's spend above tier 0 (seeded from the
  ledger file on first use, then added to live as calls are priced), and the
  audit (`routing.escalate`, `routing.capped`, `routing.set`,
  `routing.unset`).
- The lane's harness profile (context window) is the **smallest** of the
  tiers' windows, so compaction thresholds hold whichever tier answers.

## 5. Pins, fallback, escalation: how they compose

1. **Pick** (M21 precedence). A concrete model → no routing; stop here,
   M21 as today. A tier ref, or the default with routing on → a ladder
   whose floor is that tier.
2. **Level**: the ladder's current tier (floor, or higher after an
   escalation or `/model strong`), capped by `strong_daily_usd`.
3. **Outage**: if that tier's model is marked down (M21), the first
   fallback that isn't down answers instead, and the owner is told once, as
   today. The ladder doesn't move: once the model is back, the tier is.

So a turn escalated to `strong` while `strong` is down runs on the
fallback; the ledger row says `tier: strong` with the fallback's
`provider/model`. A cheap tier that is down falls back without escalating:
an outage isn't a quality signal. A fallback that answers badly *can* then
escalate: the signal is about the answer, not the model.

## 6. Money

- Every call is priced at the model that served it (M21), so strong-tier
  calls cost what they cost in the ledger, the M19 day and task caps, the
  M12 tree budget and the eval budget. Nothing is exempt.
- `strong_daily_usd` sums the cost of today's (UTC) rows whose `route.tier`
  isn't tier 0, plus the live calls since. At or over the cap: no
  escalation (audited `routing.capped` once a day, the owner told once), and
  a floor above 0 starts on tier 0.
- Switching tier loses the prompt cache for that turn: the strong model
  reads the whole history uncached once. This is the real cost of an
  escalation, and why it's sticky. `docs/routing.md` says so.

## 7. Surfaces

- **Telegram** `/model`: the tiers (name, model, price) and what the chat is
  on; `/model strong` forces the next turn; `/model tier:strong` pins it.
- **CLI** `ferrule model route`: shows the routing config and suggests a
  pair from the M22 catalog with prices; `route set <cheap> <strong> [...]`,
  `route off`. `ferrule setup` → Models gets "Routing: start cheap, escalate
  when needed".
- **Dashboard**: a Routing section (config, escalations per day with
  reasons, spend per tier, from the ledger) and the Routing API:
  `POST /api/routing/set {tiers, de_escalate?, strong_daily_usd?}` and
  `POST /api/routing/unset`, with the same session, CSRF and audit as the
  Models API.
- **Doctor**: a tier that doesn't resolve or has no key.

## 8. Measuring it: the eval variant

`ferrule eval run <suite> --variant routing --cheap <ref> --strong <ref>`
runs every task three times with the **engineered** harness:

| variant | provider |
|---|---|
| `cheap` | the cheap model only |
| `routed` | cheap → strong, the routing described here (all triggers on, `de_escalate` irrelevant in a one-turn run) |
| `strong` | the strong model only |

Without `--cheap`/`--strong` it takes the first and last of `[routing]
tiers`. Every row is priced at the model that served it. The report adds an
"escalations" line and, for a regression suite, gates on `routed`. It never
reads the owner's routing state, like M21 §7.

The starter mock (`evals/starter/mock/model.py`) always fixes a failed
check, so it can't show an escalation. A separate script,
`evals/routing/weak_mock.py`, loads that mock unchanged and adds one flag,
`--weak`: told that a check failed, it says it's done again without fixing
anything. Cheap = weak mock, strong = the starter mock: `cheap` fails the
verify tasks, `routed` escalates on the first failed check and passes. The
starter suite, its graders and `model.py` are untouched.

## 9. Threat model

- **A prompt that makes the cheap model fail on purpose** to get strong
  answers: it costs the owner money, bounded by `strong_daily_usd` and
  M19's caps. Escalation grants nothing else: same tools, same sandbox,
  same approvals.
- **A tool result that fakes a signal** (text saying "[ferrule] fails"):
  signals come from the loop's own state (the check's exit, the hook's
  verdict, the error class), never from message text.
- **The Routing API** changes which models run and what they cost: it is
  behind the dashboard session and CSRF token, and audited, like the Models
  API. It can't add a model or a key; tiers must already resolve.
- **Nothing secret** goes into the new ledger field or audit: tier names,
  refs and reason codes only.

## 10. Failure modes

| What goes wrong | What happens |
|---|---|
| A tier ref doesn't resolve (renamed model) | One warning; routing uses the tiers that resolve; under two it's off and the default serves, as today. A pin `tier:strong` then falls back to the default with M21's warning. |
| The strong tier has no key | `route()` fails for that tier: the escalation is refused (not moved) and the call stays on the current tier, with one warning. |
| The strong model is down | M21 fallback (§5). |
| Cap reached mid-turn | The turn stays where it is; no further escalation. |
| Config edited mid-turn (tiers changed) | The ladder resets to the new floor at the next call. |
| Escalation retry fails again on the strong tier | Same rules as any call: a non-retryable error on the top tier ends the turn with that error. |
| History from the cheap tier holds thinking/`native` blocks | M23: replayed only to the same api and model; a switch sends the plain transcript. |

## 11. Out of scope

- Phases 2–3 (a learned router, local LoRA): dropped (msg 3070).
- Up-front difficulty classification of any kind.
- Escalation inside a sub-agent reaching its parent (each agent has its own
  ladder; a parent doesn't escalate because a child did).
- A verifier role verdict as a signal (§3).
- Routing the eval judge or the M16 learning pass: they run on the models
  they're told to use.

## 12. Tests (hermetic)

Two scripted mock models, weak and strong:
- each trigger fires, escalates, and the level sticks for the turn;
- the next turn de-escalates (and doesn't with `de_escalate = false`);
- `/model strong` forces one turn only;
- tier pins set the floor; a concrete pin isn't routed;
- escalation and M21 fallback compose as §5;
- `strong_daily_usd` stops escalation, seeded from the ledger;
- routing off: a golden transcript (requests, rows, events) is identical
  to a run with no routing code in the path;
- ledger rows carry `route.tier` and the reason;
- the Routing API needs the session and CSRF, and audits;
- `--variant routing` runs the three arms and prices each row at its model.

The live comparison (real models over OpenRouter) is documented in
`docs/routing.md`; it is never run by `cargo test`.

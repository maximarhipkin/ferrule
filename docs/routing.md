# Routing: start cheap, escalate when needed

Every turn starts on a cheap model. It moves to a stronger one **only when
the cheap one visibly fails**: a call it can't make, broken tool calls, a
check or a Stop hook that rejects its answer, going in circles. Nobody
guesses up front how hard a request is. The design, with the reasons for
each decision, is `docs/m25-routing.md`.

Routing is **off by default**. While it's off, nothing changes: the same
requests, ledger rows and events as before routing existed.

## Turn it on

The tiers are models you've already connected (`docs/models.md`), listed
cheap first. Any reference works: an alias, `provider/model`, a model id.

```bash
ferrule model route                      # the tiers, last 7 days' escalations, a suggested pair with prices
ferrule model route set cheap strong     # on, over these two (or more), cheap first
ferrule model route set cheap strong --cap 2   # plus a $2/day cap above the cheap tier
ferrule model route off                  # off; the tiers stay written, so tier: refs still work
```

Or in the config:

```toml
[routing]
enabled = true
tiers = ["cheap", "strong"]          # two or more, cheap → strong
de_escalate = true                   # default: back to the cheap tier at the next turn
strong_daily_usd = 2.0               # optional: per UTC day, on spend above tier 0

[routing.triggers]                   # optional; all on by default
call_failed = true
tool_errors = 2                      # 0 = off
checks = true
stop_hooks = true
no_progress = 3                      # 0 = off
watchdog = true
```

Edits take effect at the next call; no restart. The dashboard's Routing
section does the same (`POST /api/routing/set`, `/api/routing/unset`,
behind the dashboard session and CSRF token, audited).

## What makes a turn move up

| Signal | When | Ledger reason |
|---|---|---|
| a call fails | an error retrying won't fix: a malformed response, context too long, a refusal, a bad request, a model without tool support. The same request goes again at once, one tier up. | `call_failed:<class>` |
| tool errors | 2 invalid tool calls in a row (an unknown tool, arguments that aren't an object or miss a required field) | `tool_errors` |
| a check fails | the task's verify command, a `check = true` hook, or eval's verifier failed when the model said it was done | `check_failed` |
| a Stop hook | an M18 Stop hook sent the answer back | `stop_hook` |
| no progress | the same tool with the same arguments 3 times in a row, or the stuck detector's first nudge | `no_progress` |
| the watchdog | the gateway's watchdog saw the session stall; the next call moves up | `watchdog` |
| the owner | `/model strong` | `owner` |

Rate limits, 5xx, timeouts and a bad key are **not** signals: they are
outages, and retries and the fallback list handle them.

- One tier per signal. With three tiers, a second failure on the middle one
  moves up again. A signal on the top tier does nothing.
- **Sticky for the turn**: it never moves back down inside a turn.
- **Back down at the next turn** (`de_escalate = true`). A scheduled task
  run is one turn, so it's sticky for the task. With `de_escalate = false`
  (`route set … --sticky`) it stays up for the session.
- Routing only changes **which model answers next**, never what it's told:
  the failed check still goes back to the agent, the stuck nudge is still
  sent. Escalation grants no tools or permissions.

## Pins, tiers and `/model strong`

A tier ref works wherever a model word does: a chat pin, a task's model, a
role's `model`, `spawn_agent(model)`. `tier:cheap`, `tier:strong` (the
last), `tier:0`, `tier:1`, …

- A tier ref sets the **floor**, where the turn starts. `tier:strong` on a
  role makes a planner that starts strong; `tier:cheap` makes workers that
  start cheap. Each sub-agent has its own tier state.
- A **concrete model** (a pin, `--model`, a task's or role's model that
  isn't a tier ref) is not routed: you asked for that model.
- In Telegram: `/model tiers` shows the tiers, `/model strong` puts **the
  next turn** on the top tier (once), `/model tier:strong` pins the chat.

## With the fallback list

Escalation picks the tier; M21's fallback then applies to that tier's model
as always. If the strong tier is down, an escalated turn runs on the first
fallback that isn't, and its ledger row says `tier: <strong>` with the
fallback's model. A cheap tier that is down falls back without escalating.

## Money

- Every call is priced at the model that served it, so strong calls count
  in full against the ledger, the daily and task caps, a sub-agent tree's
  budget and the eval budget.
- `strong_daily_usd` caps today's (UTC) spend above tier 0. Past it, nothing
  escalates and a tier floor above 0 starts on tier 0, until tomorrow. You
  get one note, and the audit log gets `routing.capped`. Concrete pins aren't
  limited by it.
- **An escalation loses the prompt cache for that turn**: the strong model
  reads the whole conversation uncached once. That's the real price of
  moving up, and one reason it's sticky.

## Seeing it

- `ferrule ledger`: routed rows carry `route: {tier, escalated}`, where
  `escalated` is the reason, on the first call after a move.
- `ferrule model route` and the dashboard's Routing section: the tiers,
  escalations per day by reason, spend per tier.
- `ferrule doctor`: routing on or off, today's spend against the cap, a
  tier that doesn't resolve or has no key.
- The audit log: `routing.escalate`, `routing.capped`, `routing.set`,
  `routing.unset`.

## Measuring it

`ferrule eval run <suite> --variant routing` runs every task three times,
all with ferrule's harness:

| variant | model |
|---|---|
| `cheap` | the cheap model only |
| `routed` | cheap → strong, as above, every trigger on |
| `strong` | the strong model only |

`--cheap <ref> --strong <ref>` name the pair; without them it takes the
first and last of `[routing] tiers` (config only, never a chat's state).
Every row is priced at the model that served it. The report adds an
escalations line, and a regression suite's exit status gates on `routed`.
The strong model grades rubrics unless `--judge-provider` names another.

### 1. With no model (free)

`evals/routing/weak_mock.py` is the starter mock, loaded unchanged, plus a
`--weak` flag: told a check failed, it says it's done without fixing
anything. Weak as cheap, the plain mock as strong:

```bash
python3 evals/routing/weak_mock.py --port 8765 &          # strong
python3 evals/routing/weak_mock.py --port 8766 --weak &   # cheap
cat > eval-routing-mock.toml <<'EOF'
default_provider = "mock"

[providers.mock]
base_url = "http://127.0.0.1:8765/v1"
api_key_env = "MOCK_KEY"
model = "mock"
price_input_per_mtok = 1.0
price_cached_input_per_mtok = 0.1
price_output_per_mtok = 5.0

[providers.weak]
base_url = "http://127.0.0.1:8766/v1"
api_key_env = "MOCK_KEY"
model = "mock"
price_input_per_mtok = 0.1
price_cached_input_per_mtok = 0.01
price_output_per_mtok = 0.5
EOF
MOCK_KEY=x ferrule --config eval-routing-mock.toml eval run evals/starter \
  --variant routing --cheap weak/mock --strong mock/mock
kill %1 %2
```

Expect `cheap` 16/20 (it fails the four verify tasks), `routed` 20/20 with
4 escalations, all `check_failed`, and `strong` 20/20, at $0.05, $0.06 and
$0.53 with these made-up prices (routed pays the strong price only for the
calls after each escalation). With
`--tag smoke`: 3/4, 4/4, 4/4 and one escalation (slugify). That shows the
mechanism works; it measures nothing about real models.

### 2. The real comparison

Connect both models (`docs/models.md`) with prices, so the dollar cap
works, then dry-run and a smoke run before the full suite:

```bash
ferrule eval run evals/starter --variant routing --cheap <cheap> --strong <strong> --dry-run
ferrule eval run evals/starter --variant routing --cheap <cheap> --strong <strong> --tag smoke --max-usd 2
ferrule eval run evals/starter --variant routing --cheap <cheap> --strong <strong> --repeat 3 --max-usd 15
```

**Expected cost.** Three arms is 1.5× an A/B. The mock's full A/B moved
about 1M tokens; real models take more steps, so plan for roughly 1–3M
tokens per arm on the full suite, input-heavy. `cheap` costs that at the
cheap price, `strong` at the strong price, and `routed` about the cheap
arm plus the escalated tasks at the strong price. For a strong model at
$3/$15 per M tokens and a cheap one at a tenth of that, one full repeat
lands around $5–15; the smoke subset around $1–2. `--dry-run` prints the
ceiling for your pair. The default cap is $5, so the full suite needs
`--max-usd`; ask the owner before going above $10.

What to read in the report: the pass rates of `routed` against `strong`
(how much quality routing keeps), the cost of `routed` against `strong`
(what it saves), and the escalations line (why it moved up).

### 3. The live test

The same smoke run as an `#[ignore]`d test, capped at $1. It needs a config
with `[routing] tiers` and the keys in the environment:

```bash
FERRULE_LIVE_CONFIG=~/.config/ferrule/ferrule.toml \
  cargo test -p ferrule-cli --test eval -- --ignored live_routing --nocapture
```

`cargo test` never runs it.

# M19b: reliability — never silently deaf (design)

Status: design, 2026-09-25. Follows M19 (trust & cost) and the Telegram
hotfix (PR #8). Where the build departs from this, the PLAN log says so.

## Why

The owner runs `ferrule gateway` on a server that accepts no inbound
connections. When the bot went quiet on Telegram, the only way to learn why
was SSH. From a phone alone the owner must always be able to tell

1. that a message **arrived**,
2. what the agent is **doing**, or where it is **stuck**,
3. that the whole process is **down**, and to recover.

PR #8 made the Telegram adapter itself robust (deadlines, retry, backoff).
What was left:

| failure | what the owner saw before | what M19b adds |
|---|---|---|
| message queued behind a long turn | nothing, for as long as the turn runs | 👀 at once, and one "busy with …, queued" reply |
| a turn hangs (model or tool never returns, no progress) | nothing, forever; the chat's lane is blocked | a watchdog message to the owner chat; `max_turn_minutes` ends the turn |
| "is it alive, what is it doing?" | SSH | `/status` from any allowed chat, answered without the model or the lane |
| crash or OOM kill mid-turn | the turn's reply never comes, nobody says why | a restart notice naming the interrupted turn |
| the process is wedged (alive, but the dispatcher or polling is stuck) | nothing; systemd sees a live pid | systemd's watchdog restarts it |
| the process or the box is down | nothing | an optional outbound heartbeat, so an external checker alerts |

## The pieces

### 1. Receipt ack, outside the lane

The gateway reacts 👀 (Telegram `setMessageReaction`) to every message it
admits for a lane, **before** queueing it, so the reaction doesn't wait for
the turn in front. Adapters without reactions skip it; Telegram's
`ChannelCapabilities` now says `reactions: true`. The reaction is
best-effort with a short deadline: a failure is logged and never delays or
drops the message. Commands the gateway answers itself (`/stop`, `/status`,
approvals) get their reply instead.

If the lane is already running a turn, the chat also gets one reply:
"busy with <what> for N min, your message is queued — /stop to cancel it".
At most one per busy period per chat: the flag clears when the lane goes
idle with an empty queue. Scheduler runs and wakes get neither.

### 2. `/status` and `ferrule status`

A gateway interceptor answers `/status` itself: no model call, no lane,
from any allowed chat, while a turn hangs. (The gateway now asks a list of
interceptors in order; M19's owner door is the first.) It reports

- version, uptime, the start time and whether the last exit was unclean;
- each busy lane: the chat, what it's doing (a model call, or a tool name
  with a short argument summary), for how long, since the last progress,
  and its queue depth;
- spend today against the caps (M19's `trust status` lines);
- the next scheduled runs and the last failed one (M3's store);
- channel health: each polling channel's last successful poll;
- the last few warnings and errors, from an in-memory ring buffer fed by a
  `tracing` layer.

`ferrule status` prints the same on the box: the daemon writes the report
to `<data>/gateway/status.txt` every few seconds, and the CLI reads it. No
marker means "no ferrule gateway is running"; a stale report says the
daemon may be wedged and names its pid.

Secrets stay out. There is no shared redaction function yet, so M19b adds
one (`ferrule_gateway::Redactor`): the values of the configured secret env
vars (`[secrets]`, the Telegram token, provider API keys) and anything
shaped like a Telegram bot token become `[redacted]`. It runs over the ring
buffer, `/status`, the status file, the heartbeat and the restart marker.
Telegram errors drop their URL (it carries the token).

### 3. Turn watchdog and `max_turn_minutes`

Each lane's agent events (already emitted, until now dropped) are drained
by a small task that records the current activity and the time of the last
progress (a model call starting or finishing, a tool starting or
finishing). A gateway task checks the busy lanes every few seconds: a lane
with no progress for `watchdog_after` (default 10 min) sends the owner
chat **one** message, "stuck on <X> for N min — /stop to cancel". It fires
again only after progress resumes and stalls anew.

`max_turn_minutes` (default 60, 0 = off) wraps the turn's guard in a
deadline: past it the turn ends exactly the way `/stop` ends one (M19's
halt, which also interrupts a running model or tool call), the chat gets a
clear final message, and the lane takes its next message. It covers the
lane's root agent; sub-agents already run under their parent's own tools
and timeouts.

The existing ceilings stay: `shell` kills its process group after 120 s by
default (the model can't raise it), the provider has its request timeout,
and MCP calls theirs. The watchdog is for what those miss.

### 4. Restart notice

While the gateway runs it keeps `<data>/gateway/running.json`: its pid,
start time, and each lane in a turn with the (redacted) message it is
handling. A clean shutdown (SIGTERM, Ctrl-C) removes it. At start, a
marker left behind means the last exit was unclean: the owner chat is told
once, "I restarted at T; the turn for <chat> was interrupted while
handling: '<first 80 chars>'". The turn is **not** re-run: it may have
been half-done, and re-running a tool call is worse than asking. A clean
`systemctl restart` says nothing, or "back up" if `notify_on_start` is on
(default off).

### 5a. systemd watchdog

The unit `ferrule setup` writes gets `WatchdogSec=120` and
`NotifyAccess=main`. The daemon sends `WATCHDOG=1` to `NOTIFY_SOCKET` (a
raw unix datagram, abstract or path; no crate) every third of
`WATCHDOG_USEC`, **only while** the dispatcher isn't stuck on one message
and every polling channel has polled successfully within
`poll_stale_secs` (default 300; Telegram long-polls every ~30 s). When it
stops, systemd kills and restarts the service, and piece 4 tells the owner.
The first `poll_stale_secs` after start count as healthy, so a slow first
poll doesn't cost a restart. Units written by older versions have no
`WatchdogSec`, `WATCHDOG_USEC` is unset, and this is a no-op. Linux only.

### 5b. Outbound heartbeat

```toml
[health]
heartbeat_url = "https://hc-ping.com/<uuid>"
heartbeat_secs = 60
```

Every `heartbeat_secs` the daemon POSTs
`{"status":"ok"|"degraded","reason":"…","version":"…","uptime_secs":N}`.
Degraded means a stale channel, a stuck lane, or the kill switch on; the
reason is built from fixed phrases and chat ids, never message text,
tokens or URLs. When the process or the box dies the pings stop and the
checker (healthchecks.io today, M20's Cloudflare Worker relay later)
alerts. Outbound only; a failing ping is a warning, never fatal.

## Defaults

```toml
[health]
watchdog_after_secs = 600     # "stuck" message after 10 min without progress; 0 = off
max_turn_minutes = 60         # a turn is ended after this; 0 = off
notify_on_start = false       # "back up" after a clean start
heartbeat_url = ""            # off
heartbeat_secs = 60
poll_stale_secs = 300         # a channel counts as stale after this
```

The unit: `WatchdogSec=120`.

## Eval stays hermetic

`ferrule eval` doesn't run the gateway, so none of this is wired there: no
reactions, watchdog messages, heartbeats, sd_notify or restart marker. A
binary test runs a suite with a heartbeat URL, a Telegram mock and a
`NOTIFY_SOCKET` all set, and checks that nothing is hit or written.

## Out of scope

- Inbound access of any kind (a web dashboard, webhooks): M20's relay.
- Re-running an interrupted turn automatically.
- Watchdogs for `ferrule chat` and `ferrule run` (a person is watching).
- Deadlines on sub-agents separate from their root's turn.
- launchd/Windows service watchdogs: sd_notify is Linux-only.

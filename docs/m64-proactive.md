# M64 — a proactive agent

The owner's words: "I need the agent to be proactive. The ideal is he
will work by himself, will understand what he needs to do from what he
learns from the user."

Until M64 ferrule only answered: a chat message, a scheduled task, a
webhook. M64 adds a pass that looks at what the owner has said and what
is coming up, and decides on its own whether to say something, offer
something or do something small, within every gate and cap ferrule
already has. It is the owner's only, off until the owner turns it on, and
silent unless it has something worth a message.

Status: designed and built on `autopilot` (2026-10-21 and 22), one
commit per task (M64.0 the gate, M64.1 this design, M64.2 commitments,
M64.3 patterns, M64.4 the heartbeat, M64.5 feedback, `/proactive` and the
page). Nothing here has run against a live bot or model.

## What others do

- **OpenClaw** had a heartbeat and has folded it into ordinary scheduled
  jobs: "Heartbeat monitoring now uses ordinary automation jobs." Setup
  "provisions one enabled **Proactive check** job for the ambient owner
  agent", which "runs every 30 minutes unless the provider supplies a
  different initial cadence". A check that has nothing to say returns
  `NO_REPLY`; `activeHours` "restricts execution to a timezone-aware
  window"; `payload.skipIfScratchEmpty` skips a check with nothing in its
  scratch; `delivery.target: "owner"` sends to the owner's DM only
  ([docs.openclaw.ai/gateway/heartbeat](https://docs.openclaw.ai/gateway/heartbeat),
  read 2026-10-21).
- **Hermes** schedules through cron jobs the agent manages itself, with
  self-healing re-runs of a missed fire
  ([v0.22.0](https://github.com/NousResearch/hermes-agent/releases/tag/v0.22.0),
  in `research_notes/…/personal_agents_landscape.md`); the notes found no
  heartbeat of its own.

What ferrule takes from that: the pass is a built-in scheduled task, not
a loop of its own; silence is the default answer; a pass with nothing new
skips the model call (OpenClaw's empty-scratch skip); delivery is to the
owner alone; a time window. What ferrule adds: the trust gates and
dollar caps, a level the owner picks (off, suggest, act), buttons on
every idea, a per-kind "Never" list, and commitments and patterns learned
from the owner's own words.

## 0. The gate comes first (M64.0)

An agent that acts on its own needs the gate it acts through to be
whole. M63.1 found that a `curl -X POST` to a host with a bound key (the
Home Assistant skill's light switch) wasn't gated; only a DELETE was.
M64.0 made a POST, PUT or PATCH to a bound host leave the box: asked
about after outside content in every mode, refused when nobody can be
asked ([m19-trust-cost.md](m19-trust-cost.md) §6b, "HTTP writes to a
bound host"). The proactive pass itself never runs a command or a tool
(§3), so it never reaches that gate; a task it schedules does, like any
other task.

## 1. What it learns

From the **owner's own words only**: what the owner writes in their own
private chat or on the dashboard's chat. Guests' chats, groups and
everything a tool fetched never become a commitment, a pattern or a
preference.

- **Commitments and deadlines** the owner states: "I need to send the
  deck to Dana by Friday", "remind me to call the bank", "I'll renew the
  passport next week". Each has its text (the owner's words, shortened),
  a due time when one was said, the request it came from, and a status
  (`open`, `done`, `dropped`, `snoozed` until a time).
- **Open loops**: something started and not finished. Two sources: what
  the owner says ("I started the tax form, finish it tomorrow"), and the
  owner's scheduled tasks whose last run failed or stopped incomplete
  (the scheduler's run log, as M16's learning pass reads it). A loop is a
  commitment of kind `loop`.
- **Recurring patterns**: the same request at similar times or days
  ("what's the weather" most mornings), or the same kind of task added
  again and again (a one-off reminder made every week). Found by
  counting, not by a model (§4).
- **Preferences**: M56.4 already keeps them, as memory facts tagged
  `profile` or `preference` ([memory.md](memory.md)). The pass reads up
  to 10 of them; it doesn't store its own.

### How commitments are extracted, and why that way

Three ways were weighed:

| Way | Model calls | Prompt prefix | Languages | Delay |
|---|---|---|---|---|
| A `note_commitment` tool the chat model calls | none extra, but each call is one more round trip of the whole conversation | a tool schema in every owner request | any | none |
| A cheap post-turn pass | one per owner message, or a keyword filter in front of it | unchanged | the filter's (English) | none |
| **Batched into the heartbeat's one call** | **none extra**: the pass already calls the model when anything changed | **unchanged** | any | until the next pass (3 h by default) |

**Chosen: batched into the heartbeat.** The pass already makes one cheap
call whenever its inputs changed, and new owner messages are an input,
so extracting commitments from them costs a few hundred more input
tokens on a call that happens anyway. It needs no keyword filter (the
owner may write in Hebrew or Russian), adds nothing to the chat's prompt
prefix (which the eval gate hasn't measured, M58.5), and costs nothing
on a day with no new messages. The delay is acceptable because an
explicit "remind me at 9" is already the chat model's job: it schedules
the task at once (`ferrule_admin` `task_add`, M56.3's no-card op).

The door (`ProactiveDoor`, last of the gateway's interceptors) records
each plain message the owner sends in their private chat or the
dashboard's chat (not a command, not an approval reply, which earlier
doors take) with its time, while proactive is on. That log is also what
the patterns are counted over: transcripts have no timestamps.

On top, a **`commitments` tool** for the owner's chats, present only
while proactive is on (a one-time prefix change when it's turned on):
`list`, `add` (when the owner asks to track something), `done`, `snooze`
and `drop`. And a `/commitments` command that lists them, without a
model.

## 2. The store

`<data>/proactive/proactive.db` (SQLite), the owner's only: written by
the door, the pass, the tool in owner sessions and the owner's commands.
The directory is in `owner_mode::guest_hidden`, so no other chat's
commands or file tools can read it, in any mode.

| Table | What |
|---|---|
| `requests` | the owner's messages: time, session, text (clipped to 500 characters); kept 30 days |
| `commitments` | kind (`commitment`, `loop`), text, due, the request or task it came from, status, snoozed until, the reminder task made for it |
| `items` | each thing the pass noticed, said, suggested or did: its kind, text, mode, the action, its basis (`owner` or `outside`), the message it went out in, the outcome (`done`, `not_now`, `never`, `undone`, `up`, `down`, `failed`) |
| `suppressed` | the "Never" list: a kind, when, from which item |
| `state` | the last pass (when, what it did), the digest of its inputs |

## 3. The heartbeat

A built-in scheduled task, `ferrule-proactive`, like the learning pass
and housekeeping: listed by `ferrule tasks`, pausable, its runs logged.
It runs every `every_hours` (3 by default) at minute 7, in the owner's
timezone (`[proactive] timezone`, else `[trust] timezone`). Turning
proactive on or off from the chat or the page adds or removes it at once.

It never runs an agent turn, a command or a tool that changes anything:
it reads, makes one model call, and writes to its own store, the
scheduler and the owner's chat. Each pass, in order:

1. **Off?** `enabled = false`: skipped, nothing read.
2. **Quiet hours** (`quiet_hours`, default `22:00-07:00` in the owner's
   timezone): skipped, no model call. Also skipped, with the reason in
   the run log: the kill switch or a spent `[trust]` day cap
   (`Hub::check`), today's proactive dollars spent (`max_usd_per_day`),
   and, when the level sends messages, today's message cap or the
   minimum gap since the last proactive message. A skipped pass leaves
   everything for the next.
3. **Inputs**, each with a truthful status when it can't be had ("no
   calendar connected", "mail: Gmail answered 401"):
   - the owner's profile and preference facts (M56.4, `memory.db`);
   - the open commitments and loops, with their due times;
   - the owner's messages since the last pass (for new commitments);
   - pattern candidates (§4);
   - the owner's scheduled tasks and their last runs (failed or
     incomplete ones are loops);
   - the next 24 hours of each connected calendar (native CalDAV, `.ics`
     and Google connections' read tools, called in-process) when
     `calendar = true`; **outside content**;
   - unread mail headers from the last day (a native Gmail or IMAP
     connection's search: sender, subject, date, never a body) when
     `mail = true`; **outside content**;
   - the pass's last ten items and their outcomes, and the "Never" list.
4. **Nothing changed?** The inputs' digest (everything but the clock)
   is compared with the last pass's. Equal, or no inputs at all: no
   model call. A quiet day costs nothing.
5. **One model call** (§5's cheap model), a fixed system prompt (so the
   prefix caches) and the inputs as JSON, outside content fenced and
   labelled as data. The answer is JSON: new commitments (each naming
   the request it came from), commitments the new messages show done,
   ideas (each naming what it's about: a commitment, a pattern, a loop,
   the calendar, the mail), and an optional short note. Not JSON: "the
   model's answer couldn't be read; nothing was sent", logged.
6. **Decide** (ferrule, not the model): what an idea may do is fixed by
   what it refers to (§4, §6). A suppressed kind, or one cooling down
   after "Not now", is dropped; at most one message goes out per pass.
7. **Deliver** to the owner's primary chat through the hub's notifier,
   waiting for the channel's answer, so a failure is the run's status,
   not a silence.
8. **Audit**: `proactive_pass` (what it read, what it decided, what it
   cost or why it skipped), `proactive_noticed`, `proactive_suggested`,
   `proactive_done`, `proactive_answered`, `proactive_feedback`,
   `proactive_suppressed`, `proactive_cleared`, `proactive_undone`,
   `proactive_settings`, `proactive_said`, `proactive_commitment`. The
   model call is a ledger row (`call_kind = "proactive"`), so it counts
   in every `[trust]` cap.

## 4. Patterns

Counted over the last 28 days of the owner's messages, plus the owner's
one-off tasks (their prompts at their times):

1. Each text becomes a set of words: lower-cased, punctuation dropped,
   English stop words and words of one or two letters left out (other
   languages' words are kept as they are).
2. Greedy clustering: a text joins the first cluster whose first text
   shares at least half the words (Jaccard ≥ 0.5).
3. A cluster is a pattern with **3 or more texts on 3 or more different
   days**. Its time: the median local time, when every text is within
   90 minutes of it; its days: every day, Monday to Friday, or one
   weekday, as the dates show. No common time: not a candidate.
4. Left out: a pattern an existing task of the owner's already covers
   (its prompt shares half the words), one suggested in the last 30
   days, one on the "Never" list.

A candidate goes to the pass with ferrule's own proposal: a cron line
(`0 8 * * *`), a name and a prompt (the owner's most recent wording).
The model only phrases the question ("You ask for the weather most
mornings; send it at 08:00 every day?"); the schedule and the prompt
are ferrule's, so nothing the model read can change them.

## 5. Restraint

- **Off by default.** `enabled = false`.
- **Quiet hours**, in the owner's timezone: no pass at all.
- **A daily cap** on proactive messages (`max_messages_per_day`, 3), and
  **a minimum gap** between them (`min_gap_minutes`, 120). Command
  replies and reminders the owner set up don't count.
- **One message per pass**, at most.
- **A dollar cap per day** for the passes (`max_usd_per_day`, $0.25),
  inside the `[trust]` caps, which still apply. A model with no prices
  counts as $0 here (the trust caps' token limits still hold), and the
  page says so.
- **A cheaper model**: `[proactive] model` (any model reference or
  alias, such as `cheap`), else `[routing]`'s first tier when routing is
  on, else the default model.
- **No call without a change** (§3, step 4).

## 6. Autonomy levels

`[proactive] level = "off" | "suggest" | "act"`; turning it on starts at
`suggest`.

| Level | What happens |
|---|---|
| `off` | The pass notices (commitments, loops, patterns appear on the page and in `/proactive`) and says nothing. |
| `suggest` | Every idea is a question with **Do it / Not now / Never**. A note (a meeting at 9, an unread mail from the bank) comes with **Useful / Never**. |
| `act` | Low-risk actions are done, each with a receipt and **Undo**; everything else is still suggested. |

**What "low-risk" is**, and nothing else:

- a **reminder** for a commitment the owner stated, at a time before its
  due time (09:00 on the day when only a day was said, an hour before a
  stated time);
- **scheduling one of the owner's own tasks** from a pattern (§4), the
  same task M56.3's no-card `task_add` makes, answering in the owner's
  chat with no gate script;
- a note or summary to the owner.

Never done by itself: anything whose basis is outside content (an idea
from the calendar or the mail is always a suggestion, and the most it
can offer is a reminder, never a task), and anything else at all. **Do
it** on a suggestion runs the same fixed action. Reminders are
**built-in reminder tasks** (`ferrule-reminder`, a one-off in the
scheduler): at their time the scheduler hands their text to the owner's
chat as is, with no model call. A calendar invite's title can't become a
prompt that runs as the owner's task. Undo deletes the task the action
made, from the receipt's button or `/proactive undo <id>`, and survives a
restart (it's in the store).

**Nothing bypasses the gates.** The pass runs no tool; a task it makes
runs later as the owner's scheduled task, under every gate, grant, the
untrusted-send gate and the caps, exactly as one the owner made.

## 7. Learning from feedback

- **Buttons**: Do it, Not now, Never, Useful and Undo are recorded on
  the item (`proactive_answered`).
- **👍/👎** on a proactive message (Telegram's reactions, through M53.5's
  plumbing: the channel's reaction sink, now asked for when learning or
  proactive is on) are recorded on its items, the owner's only.
- **Never** puts the item's kind on the suppression list for good: a
  pattern's own key (`pattern:weather`), or a kind (`reminder`, `loop`,
  `calendar`, `mail`, `note`). The list is on the page and in
  `/proactive`, each entry clearable.
- **Not now** (and 👎) slow that kind down: after the n-th in 30 days
  (since the last 👍 or Useful on that kind), the kind waits 2^(n-1) days
  (at most 16) before the pass offers it again. **👍** and **Useful** end
  the wait.
- The pass's prompt shows the last items and their outcomes, so the
  model sees what the owner liked and waved off.

## 8. Visibility

- **The dashboard's Proactive page**: on/off and the settings (level,
  interval, quiet hours, caps), the last pass and the next, today's
  messages and spend, the open commitments (done, snooze, drop), what it
  noticed, suggested and did with each outcome, and the suppression list
  (clear one, clear all). "Run a pass now" asks the scheduler to run it.
- **`/proactive`**: `status` (the default), `on`, `off`, `level
  off|suggest|act`, `never` (the list) or `never <kind or id>` (add one),
  `clear [<kind>|all]`, `patterns`; the buttons send `/proactive
  do|later|never|useful|undo <id>`. The owner only.
- **`/commitments`**: the open ones; `/commitments done|snooze|drop <id>`.

## 9. Defaults and turning it on

```toml
[proactive]
enabled = false            # the owner turns it on: the page, /proactive on
level = "suggest"          # off | suggest | act
every_hours = 3            # 1–24
quiet_hours = "22:00-07:00"  # owner's timezone; "" for none
# timezone = "Asia/Jerusalem"  # unset: [trust] timezone
max_messages_per_day = 3
min_gap_minutes = 120
max_usd_per_day = 0.25
# model = "cheap"          # unset: [routing]'s first tier, else the default
calendar = true            # the next 24 h of connected calendars
mail = true                # unread mail headers from connected mailboxes
patterns = true
```

One step on: the page's switch or `/proactive on` (it writes `enabled =
true` and adds the task). `ferrule setup` doesn't ask (the wizard's
questions are unchanged). The chat model can't change these settings:
`[proactive]` isn't in `config_set`'s form, and the commands are read by
the door, from the owner, never by the model.

## 10. Safety

- **The owner's only.** The door records only the owner's private chat
  and the dashboard's chat; the tool exists only in owner sessions
  (`owner_mode::owner_tree`); the pass reads `memory.db`, never a guest's
  memory file, and sends only to the owner's primary chat. Guest content
  never enters the pass, and the store is hidden from guests' commands.
- **Outside content is data.** Calendar text and mail headers are fenced
  and labelled in the prompt, and what an idea built on them may do is
  capped by ferrule (a suggestion, at most a plain-text reminder), not by
  the model.
- **Gates and caps.** No tool runs in the pass; what it schedules runs
  later under every gate. The pass is held by the kill switch and the
  `[trust]` caps, and has its own dollar cap.
- **Audit.** Every pass, item, answer, reaction, suppression and undo is
  in the trust audit log (`proactive_*`).
- **Never anyone but the owner.** The pass has no way to message anyone
  else; a task it makes answers in the owner's chat.

## 11. Limits, known

- Remote MCP calendars and mailboxes (an OAuth MCP server) aren't read;
  only native connections are called in-process.
- A commitment waits for the next pass to be noticed (up to
  `every_hours`).
- Patterns need timestamps, so they count only messages from when
  proactive was turned on, and one-off tasks.
- 👍/👎 are read on Telegram only (M53.5's limit); the buttons work on
  every channel with buttons, and as text elsewhere. Telegram is asked
  for reactions when the gateway starts with proactive (or learning) on:
  turned on later from the page or `/proactive on`, 👍/👎 count from the
  next restart, and `/proactive status` says so.
- The page's switch and `/proactive` make the pass's task follow at
  once; `[proactive]` changed in the Config page's raw editor or in the
  file is read by each pass, but the task is only added or removed at the
  next start.
- Nothing has run live: not against a model, a bot, a calendar or a
  mailbox.

## Built: M64.2, commitments and open loops

- `ferrule-cli/src/proactive/`: `store.rs` (the SQLite store of §2),
  `door.rs` (`ProactiveDoor`), `tool.rs` (`commitments`), `reminder.rs`,
  `loops.rs`, and `mod.rs` (`[proactive]`, the due-time and reminder-time
  rules, `offer_reminder`).
- **The door** is the gateway's last interceptor. While proactive is on
  it keeps each plain message the owner sends in their private chat or
  the dashboard's (`self_service::offered`, `trust::by_owner`), clipped
  to 500 characters; messages older than 30 days are dropped as new ones
  come. It answers `/commitments` (`all`, `done|drop <id>`, `snooze <id>
  [days]`) and the buttons' `/proactive do|later|never|useful|undo <id>`,
  the owner's only. The rest of `/proactive` comes with M64.5.
- **The tool** is registered in the owner's private chat and the
  dashboard's chat while proactive is on; it is deferrable (M57.1), so
  with deferral on only its name and one line are in the prompt.
- **A commitment with a due time** gets its reminder offered at once
  (`offer_reminder`): at `suggest`, a separate question in the chat it
  was said in, with Do it / Not now / Never; at `act`, the reminder is
  made and a receipt with Undo goes out ("Done without asking: …"); at
  `off`, nothing. The reminder's time follows §6 (09:00 on a named day,
  an hour before a named time, a quarter of an hour when that has
  passed). An offer made in the chat the owner is typing in doesn't wait
  for quiet hours or count against the pass's caps: the owner just said
  it. "Never" on one puts `reminder` on the Never list.
- **Reminders** are built-in one-offs named `ferrule-reminder`
  (`reminder::create`): the job (`reminder::register`, registered whether
  proactive is on or not) posts "Reminder: <text>" to the owner's primary
  chat and waits for the channel, so a failed delivery is a failed run.
  `TasksAdmin::delete` deletes a reminder (other built-in tasks are still
  only paused), so the Tasks page's Delete works on one.
- **Loops** (`loops::sync`): one of the owner's own tasks (answering in
  an owner chat or the dashboard's, not built in) whose last run failed
  or was cut short gets one open loop, closed when a later run succeeds.
  Brought up to date when `/commitments` or the tool lists them (and by
  each pass, M64.4).
- **Delivery that waits.** `Hub::post_in` sends to an owner chat and
  waits for the channel's answer, with the message's id when the channel
  gives one: `Notifier::post_choices` (default: `send_choices`, no id),
  `ferrule_gateway::post_with_buttons` and `Channel::send_buttons_with_id`
  (default: `send_buttons`, no id; Telegram returns the id). Small
  additions to M51's Telegram send branches and the gateway's channel
  trait.
- `<data>/proactive` is in `owner_mode::guest_hidden`.
- `[proactive]` is read and checked at load (`every_hours` 1–24,
  `quiet_hours` a `HH:MM-HH:MM` range, the zone, the dollar cap, a model
  that isn't empty); the example config has the block.

## Built: M64.3, patterns

- `proactive/patterns.rs`: `candidates` counts what §4 says. The words:
  lower-cased letters and digits; English stop words, bare numbers and
  ASCII words of one or two letters dropped (a short word in another
  script is kept). A text joins the first cluster whose first text it
  shares half the words with. A candidate needs 3 texts on 3 local days,
  every one within 90 minutes of their median time of day; it is
  scheduled at that median rounded down to the quarter hour (a little
  before the owner usually asks): on that weekday when every day was the
  same weekday, on weekdays (`1-5`) when there were 4 or more days and
  all were Monday to Friday, else every day. Its key is its three
  commonest words, sorted (`pattern:forecast-weather`); its task's name
  is the key in words and its prompt the owner's latest wording. Left
  out: one an enabled recurring task of the owner's already answers (half
  the words shared with the task's prompt), one on the Never list, one
  offered or noticed in the last 30 days.
- The owner's one-off tasks count too, at the time each was set for.
- `proactive::offer` is the one way an item reaches the owner (the
  reminder offers of M64.2 go through it now): noticed at `off`; at
  `act`, an action built on the owner's own words done, with a receipt
  and Undo; anything else asked with Do it / Not now / Never. Do it on a
  pattern adds the task with `TasksAdmin::add`, answering in the owner's
  primary chat (audited `task.add`, by the owner's button or "proactive
  (act)"); Undo deletes it.
- `/proactive patterns` (the owner's): the candidates now, up to three
  offered as questions (always asked, never done unasked: the owner
  asked to see them), with a template question
  (`Pattern::question`). The pass (M64.4) offers them by itself, its
  model phrasing the question.

## Built: M64.4, the heartbeat

- `proactive/heartbeat.rs`: the built-in task `ferrule-proactive`
  (`7 */N * * *` in the owner's zone; made to match `[proactive]` at each
  gateway start, its job registered whether it's on or not) and `pass`.
- **Before the model** (`hold`), in order: off; quiet hours (a range that
  wraps midnight works); the kill switch and the `[trust]` caps
  (`Hub::check("proactive")`); today's proactive dollars, summed from the
  ledger's `proactive` rows since the owner's midnight (an unreadable
  ledger fails the run: fail closed); and, unless the level is `off`,
  the day's message cap and the gap since the pass's last message. A
  reminder offered in a chat turn (M64.2) doesn't count here.
- **Inputs** (`gather`), each with a status in the run log: up to 10
  profile and preference facts; up to 40 of the owner's messages kept
  since the last pass; up to 30 open commitments and loops (after
  `loops::sync`); up to 5 pattern candidates; the owner's tasks with
  their last run; the pass's last 10 items from 14 days with their
  outcomes; the Never list; the calendar and the mail (`outside.rs`:
  every connected native connection with `events_list`, `calendar_list`,
  `gmail_search` or `mail_search`, called in-process with 20 seconds
  each; 2,000 characters a connection; every known secret scrubbed;
  fenced in `<outside source="…">`, a closing tag inside broken).
- **No call without a change.** The digest (FNV-1a over the inputs but
  the clock, the pass's own recent items and the Never list) equal to the
  last call's, or no inputs at all: no model call. A commitment's
  reminder still goes out then, without the model: open commitments with
  a due time and no reminder, never offered one, are offered by ferrule
  itself, first, whatever the model says.
- **The call**: `[proactive] model`, else `[routing]`'s first tier when
  routing is on, else the default model; a fixed system prompt (in the
  code, `SYSTEM`), the inputs as JSON, 1,500 output tokens at most,
  temperature 0.2. One ledger row (`call_kind`, `task_shape` and `tree`
  `proactive`), priced by the model's prices, so it counts in every cap.
  An answer that isn't the JSON asked for fails the run with that reason;
  nothing is sent.
- **What ferrule takes from the answer**: a new commitment only when it
  names one of the new messages (and isn't already open); "done" only
  for an open commitment; an idea only when it refers to something in the
  input: a pattern (its task, ferrule's), a commitment or loop (a nudge
  with no action), or the calendar or the mail (at most a plain-text
  reminder within 7 days, basis `outside`, so never done unasked). A note
  goes out with Useful / Never. Kinds on the Never list are dropped.
- **One message a pass**: the first candidate (pending reminders, then
  the new commitments' reminders, then the ideas in the model's order,
  then the note). When more are left, the digest is cleared so the next
  pass calls again; at `off` every candidate is noticed and nothing sent.
- **Audit**: `proactive_pass` for every pass (skipped and why, what it
  read, the model, the cost, what it sent), plus the item events of §3.
  The last pass is kept in the store (`last_pass`) for the page.

## Built: M64.5, feedback, `/proactive` and the page

- **The feedback store.** A `feedback` table (item, kind, time, verdict)
  in `proactive.db`, appended to by Not now (`not_now`), Useful
  (`useful`) and the owner's 👍/👎 (`up`, `down`). `Store::resting` reads
  §7's back-off from it (`store::back_off`); an item shows its last
  thumb beside its outcome. A thumb doesn't overwrite an answer (Do it,
  Not now, Never, Undo); on an unanswered item it becomes the outcome,
  and a thumb or Useful never counts as "answered already" for the
  buttons.
- **👍/👎** (`proactive/feedback.rs`). One reaction sink for the
  channels: a reaction on a message a proactive item went out in (its
  `channel`, `chat` and message id are kept on the item) is the
  proactive store's; any other goes on to M53.5's learning sink, when
  learning is on. Only the owner's single 👍 or 👎 counts (both, neither
  or one taken back: nothing); audited `proactive_feedback` with the
  kind's new wait. Telegram is asked for reactions when learning or
  proactive is on at the gateway's start.
- **The back-off in the pass.** Pending reminders wait while `reminder`
  rests; every candidate whose kind rests is held back (the run log says
  how many); the model's input gets `resting` (left out of the digest,
  like `never`) and the prompt asks it to respect it. `/proactive
  patterns` and a reminder offered in the chat the owner is typing in
  aren't held back: the owner asked.
- **`/proactive`** (the door; `proactive` is in the owner menu, 24
  commands). `status` (the default): on or off and the level, the
  interval, zone and quiet hours, the next pass (or that its task is
  paused), today's messages and spend against `max_messages_per_day`,
  `min_gap_minutes` and `max_usd_per_day`, the last pass's summary, the
  Never list, what's holding off and until when, and whether 👍/👎 are
  read. `on`, `off` and `level off|suggest|act` write `[proactive]`
  through the page's own handler (`api::act` as "the owner
  (/proactive)"), so the chat gets the page's checks; the owner typed it,
  so `act` needs no second yes. `never` lists; `never <id>` answers that
  item with Never; `never <kind>` (`reminder`, `loop`, `calendar`,
  `mail`, `note`, `pattern:<key>`) adds the kind. `clear` or `clear all`
  empties the list, `clear <kind>` takes one off (audited
  `proactive_cleared`).
- **The settings write** (`POST /api/proactive/settings`): `enabled`,
  `level`, `every_hours` (1–24), `quiet_hours`, `max_messages_per_day`
  (0–50), `min_gap_minutes` (0–1,440), `max_usd_per_day` ($0–100; 0 is no
  cap of its own). The Config page's path: the `base_hash` of the file
  the page read (409 when it changed since), the real loader's check
  (`ProactiveConfig::check` included), the guard, `.prev` and a version
  in `<data>/config-history/`, `config_saved`, plus `proactive_settings`
  with each change as `from → to`. Switching to `act` from the page asks
  first. Then `heartbeat::apply`: this process's switch, and the
  built-in task added, moved or removed (`ensure_builtin`) at once.
  No chat op maps to these routes, so the chat model can't reach them.
- **The Proactive page** (Everyday, a lightbulb): the switch, the level,
  the last and next pass, today's messages and spend, **Run a pass now**
  (the Tasks page's `tasks/run` on the built-in task); the settings form;
  the commitments, open (mark done, snooze a day, drop), snoozed and done
  lately (`POST /api/proactive/commitment`); the last 30 days of items
  with their mode, kind, basis, outcome and thumb; the Never list with
  Clear and Clear all (`POST /api/proactive/clear`, all after a yes) and
  the kinds holding off. Its words are in `lang-he.js`.


# M51 — media generation: images and video from the chat

**Status.** Built 2026-10-21. Source: `docs/research-harness2-media.md`
§4 (M51). Deviations below the design.

## What changes

The agent can **create** media, not only see it. Two chat-bound tools,
one new small crate, one built-in scheduler job, one config table.

Ferrule already has every other piece: photos come in and are seen
(M47), files go back out (`send_file`, M39 + the 2026-10-19 Telegram
work), OpenRouter is a first-class provider with a checked key (M50),
the scheduler persists one-shot and recurring jobs (M3), the ledger
prices calls (Phase 0), and `spawn_agent` can run a child on another
OpenRouter model (M21). This milestone connects them.

OpenRouter is the first media backend because it is one key for both
modalities and it normalises the shapes: `/api/v1/images/generations`
(sync, OpenAI-compatible, text-to-image and image-to-image with
reference images) and `/api/v1/videos` (async: submit → job ID → poll →
MP4), with capability discovery for both. The trait leaves room for
other backends later; none is planned.

## Parts

### 1. `ferrule-media`, the client crate

Same shape as `ferrule-embed` (M30): one trait, an OpenRouter
implementation reached **through the credential proxy**, a deterministic
`FakeMedia` for tests.

```rust
pub trait MediaBackend: Send + Sync {
    fn capabilities(&self) -> &Capabilities;          // discovery cache
    fn generate_image(&self, req: ImageReq) -> Result<ImageOut, MediaError>;
    fn submit_video(&self, req: VideoReq) -> Result<VideoJob, MediaError>;
    fn poll_video(&self, job: &str) -> Result<VideoState, MediaError>;
    fn download(&self, url: &str, dest: &Path) -> Result<(), MediaError>;
}
```

- The key comes from the provider's own config (`key_env`), exactly like
  chat and embeddings — it never enters the shell, the sandbox or a
  tool result. Media hosts are bound in the proxy like any provider
  host.
- `ImageReq`: prompt, model, aspect ratio, and optional workspace
  reference images (that is the editing path — "make this logo rounder"
  with the previous file as reference). References resolve through the
  same path rules as `read`/`write`: workspace only.
- Errors are sentences, truthful: refused key, rate limit with the
  retry-after, a model that can't take a reference image, a video job
  that died on the provider's side.

### 2. The tools, chat-bound like `send_file`

In `ferrule-gateway/src/tools/`, next to `SendFileTool`:

- **`generate_image {prompt, model?, aspect?, reference?: [paths]}`** —
  one sync call (images take seconds; the tool timeout goes to 120 s).
  Saves to `media/` in the workspace (date-stamped name), delivers the
  image to the session's chat, and returns the path for the transcript.
  In a non-chat session (`run`, sub-agents) it only saves and returns
  the path — the parent decides what to send, matching the M12 rule
  that children never touch the chat.
- **`generate_video {prompt, model?, seconds?, aspect?, first_frame?}`**
  — submits the job, writes one row to a `media_jobs` SQLite table in
  the data dir (job id, model, prompt, session's channel+chat, output
  path, state), and answers **immediately**: "making it — job
  `j_…`, usually a few minutes, I'll send it when it's done." No turn
  hangs; the model spends no tokens waiting.

Both tools say the price class in their docstrings ("video costs
dollars, not cents") so the model warns before it spends, and the first
media call of a run goes through the trust gate like any spend ("allow
for this run" covers the rest).

### 3. The poller, a built-in scheduler job

`Scheduler::with_builtin("ferrule-media", …)` (the M16 learning pass is
the precedent): every minute, read pending rows from `media_jobs`, poll
each through the proxy, and on completion download the MP4 into the
workspace, deliver it through the originating channel's send path, and
mark the row done; on provider failure mark it failed and tell the chat
in one truthful sentence. SQLite means a restart loses nothing — the
promise survives, like M48's restart promise. No model call is ever
made by the poller.

### 4. Config, caps, ledger

```toml
[media]
enabled = true
image_model = "google/gemini-2.5-flash-image"   # defaults from capability discovery
video_model = "google/veo-3.1"
max_per_day = 20          # hard cap, checked before submit
```

- With no OpenRouter-compatible provider connected, the tools are not
  registered and the Models page says why (one sentence, link to the
  connect panel).
- The cap is enforced before submit, per day, from the `media_jobs`
  table. Every submit and every delivery appends a ledger row with
  `call_kind = "media"` (precedent: `"learn"`, `"eval_result"`), cost
  filled from OpenRouter's generation pricing when the catalog knows
  it, zero and a note when it doesn't.
- `ferrule doctor` gains a media line: backend reachable, key accepted,
  today's count against the cap.

### 5. Dashboard surface (small on purpose)

- The Connect page's OpenRouter card shows "images + video" when the
  key works (discovery call, cached).
- The Models page marks media-capable models from the same discovery
  cache instead of a hardcoded list.
- `[media]` defaults are one row each on the config page. No new page.

## Security model

- Keys stay provider-side (part 1); the proxy is the only network path
  for submit, poll and download.
- Reference inputs and outputs are workspace-confined by the existing
  path rules; downloads land only under `media/` and are byte-sniffed
  (a "mp4" that isn't one is refused, mirroring M47's photo check).
- The trust gate and the daily cap are both fail-closed: a count that
  can't be read refuses the call, it doesn't wave it through.
- A media prompt is the only thing that leaves; the transcript and
  memory never ride along unless the agent writes them into the prompt
  (same exposure class as any provider call, stated in the docstring).
- Sub-agents get `generate_image`/`generate_video` without chat
  delivery (part 2), same sandbox, same proxy, shared daily cap.

## Done means

From Telegram: "draw a copper logo on dark" → the image arrives in the
chat; "make a 5-second clip of it waving" with the image attached → an
immediate job-id answer, the MP4 arrives minutes later, and
`ferrule ledger --since 1d` prices both. A day over the cap refuses in
words. Restarting the gateway mid-render still delivers. `make check`
green with: `FakeMedia` unit tests, a gateway integration test against
a mock OpenRouter (submit → poll → deliver through the fake Telegram),
a restart-during-pending test, cap and gate tests, and a ledger-row
test. Docs: this file, plus rows in `docs/dashboard.md` and
`docs/models.md`.

## Not in scope

Audio/TTS (OpenRouter has `/audio/speech`; its own milestone if wanted),
local media models, image *understanding* services beyond chat-model
vision (MCP covers OCR etc.), video editing by ffmpeg pipelines (the
shell already can, gated).

## As built (2026-10-21) — deviations from the design

- **The trust "first call per run" approval card is not in.** Spend is
  bounded by the fail-closed daily cap and the kill switch (the scheduler
  hold covers the poller); a per-run owner approval for media calls is a
  follow-up.
- **Sub-agents don't get the tools yet** (chat sessions do); "the parent
  delivers" needs a save-only variant of the tools, a follow-up.
- **The dashboard discovery card and the `doctor` media line are not
  in.** `[media]` is config-only for now; the config page edits it.
- The image tool's timeout is 180 s; video downloads get their own 120 s
  bound in the backend.
- The poller's note when a chat-less job finishes is a log line, not a
  chat message (there's nobody to tell).

## M51.4 — voice replies, OpenRouter transcription, an ffmpeg line (2026-10-21)

From the 2026-10-20 gap analysis (M51.4). Built on `autopilot`.

- **`speak {text, voice?}`**, a chat tool beside `generate_image`
  (`ferrule-gateway/src/tools/media.rs`). It calls OpenRouter's
  `POST /api/v1/audio/speech` through the same `[media]` backend: the
  provider's key as the proxy's placeholder, through the egress client,
  never in a shell. The text is at most 4,000 characters. The audio is
  saved as `media/<unix>-<n>.mp3` in the workspace, counted in the
  `media_jobs` table (kind `speech`, so `max_per_day` covers it) and
  written as a ledger row like the other media calls (`call_kind =
  "media"`, `cost_usd` empty: the speech answer is raw audio with no
  usage block). The chat's agent gets the tool, with a short
  `CHAT_VOICE` section in its system prompt, only when `[media]
  speech_model` isn't empty.
- **Voice note, no ffmpeg.** OpenRouter's speech API offers only `mp3`
  and `pcm` (`SpeechRequest.response_format` in its OpenAPI spec), no
  Ogg/Opus. Telegram's `sendVoice` takes "an .OGG file encoded with
  OPUS, or in .MP3 format, or in .M4A format" (Bot API), so the tool
  asks for MP3 and sends it as an attachment of kind `voice`; the
  Telegram channel sends an MP3 or M4A of that kind with `sendVoice`
  instead of `sendAudio`. Other channels send it by its file type. A
  `sendVoice` refusal (a 400, e.g. a user who blocks voice messages)
  goes again as a document, as every refused kind does.
- **Defaults.** `speech_model = "google/gemini-3.8-flash-tts"` (cheap and
  multilingual on OpenRouter's speech list of 2026-10-21) and `voice`
  unset, so the model picks its own; voice names are the model's (`Kore`
  for Gemini, `sarah` for ElevenLabs, `en_paul_neutral` for Voxtral).
- **Transcription through OpenRouter.** `[transcription] backend =
  "openai", provider = "openrouter"` already borrowed the URL and key;
  now a bare model name there (the default `whisper-1`) becomes
  `openai/<name>`, OpenRouter's id, and OpenRouter's `usage.seconds` and
  `usage.cost` are read: the ledger row holds the reported cost instead
  of `price_per_minute`. The request is the same OpenAI-style multipart
  upload, which OpenRouter's spec accepts. OpenAI, Groq, local servers
  and the command backend are unchanged.
- **`ferrule doctor`'s `media` line**: whether ffmpeg is on the PATH,
  its version, and whether it has the `subtitles` filter (only in a
  libass build). Informational: nothing in Ferrule needs ffmpeg; the
  agent can use it through the sandboxed shell. There is no ffmpeg tool.

**Not verified live.** No call to OpenRouter's speech or transcription
endpoint has been made with a real key, and no voice note has been sent
to a real Telegram chat. The speech call had no cost in its ledger row
(the trust caps didn't see it; the daily count did) until wave A, below. Sub-agents get the
tool only with M51.3, the media role.

## M51.3 — a `media` role for sub-agents (2026-10-21)

From the 2026-10-20 gap analysis (M51.3). Built on `autopilot`.

- **`Role::Media`** in `ferrule-agents` (`supervisor.rs`), offered in
  `spawn_agent`'s `role` enum and description, and allowed in
  `[agents.roles.media]` and graph nodes. Its child prompt says to make
  only what the task asks, look at the result, and report each file's
  path, because it can't send anything to the chat.
- **Its files land where the parent's `send_file` reaches them.** The
  supervisor never gives a media child a worktree (`place` gets `want =
  false` for it), so it works in the parent's workspace and `media/` is
  the parent's.
- **The tools.** `child_builder` (`ferrule-cli/src/main.rs`) registers
  `generate_image`, `generate_video` and, when on, `speak` for a media
  child, on the gateway's shared `[media]` handle: the same backend,
  proxy placeholder, daily cap and ledger rows as the chat's tools. The
  child's session has no chat, so the image and speech tools save and
  say "send_file can deliver it", and `generate_video` now says the
  gateway saves the clip at its path when the render finishes (it used
  to promise to send it "here", untrue without a chat). References
  resolve with the strictest hidden list (a guest's).
- **Where it can't run.** `ferrule run` and a terminal `ferrule graph`
  build children without the media handle, so a media spawn there
  fails with "a media agent needs the media tools … Spawn a worker
  instead" (the factory's error is the spawn's answer). Graphs run from
  the gateway use the gateway's supervisor, so they have it.
- **The model.** `[agents.roles.media] model = "<ref>"` is optional, as
  for every role; the report's "cheap, tool-capable" is the owner's
  pick, since no image model takes tools.

**Not verified live.** No media child has run against OpenRouter. A
video a child starts is saved, not sent: the parent learns it's there
only by looking (the poller logs it), so a parent that wants the clip
in the chat sends it itself later.

## Wave A — media spend in dollars (2026-10-21)

From the 2026-10-21 re-audit (M51.1/M51.2/M51.4 follow-ups): media rows
had `cost_usd` empty, so `[trust]`'s dollar caps never saw media spend;
`model` was free text; the default video model was the dear
`google/veo-3.1` with 20 a day.

- **Dollars in the ledger.** An image's row carries what OpenRouter says
  it charged (`usage.cost` in the answer, when the answer has it). A
  video's submit row carries its estimate (below); the delivery row
  carries the difference to the finished job's `usage.cost`, so the
  ledger's sum is the bill (negative when the estimate was high). A
  failed render's row takes the estimate back (OpenRouter's "Zero
  Completion Insurance": a failed run isn't billed); a render that
  finished but couldn't be downloaded is billed and settled the same
  way. A `speak` reply's row carries an estimate only for a model priced
  on its input alone (`pricing.completion` is `"0"` in OpenRouter's
  `/models?output_modalities=speech` list): the list price per unit
  times the characters, an upper bound, since a unit is a character or
  a token. Any other speech model (the default Gemini TTS, priced on the
  audio it makes too) is counted only.
- **Charged to the run, the day and the task.** A tool call's row is
  charged to its run tree through the trust hub (`MediaSpend`, the
  same `Hub::charge` a model call's row goes through) and written to the
  ledger, which the day and task caps read; a row carries its tree, so a
  scheduled task's media spend counts toward that task. A media `role`
  child's spend goes to its root's run. The poller's settling rows go to
  the ledger only.
- **Before a paid call.** An image or a voice reply is refused once the
  run, the day or the task has no dollars left under the `[trust]` caps
  (`Hub::usd_left`; what it costs isn't known before). A video is
  estimated first: the model's per-second list price from OpenRouter's
  `/videos/models` (`pricing_skus`; the dearest per-second price that
  names no resolution, so with audio at the default size; a `cents_…`
  key is in cents) times the asked length, or the model's longest
  `supported_durations` when none is asked (an upper bound). It's
  refused, nothing submitted, when the estimate is over `[media]
  max_usd_per_video` (default $1.00) or over what the run, day or task
  has left, and when there's no estimate (a model priced per token, a
  price list that can't be read) while either limit is set. The list is
  read through the proxy and kept for 6 hours. The answer says the
  estimate: "Making it — job … (about $0.64 for 8 s at list price)".
- **Models from the config.** `[media] image_models` and `video_models`
  list the other models a call may name; the defaults are always
  allowed. The tools' `model` parameter exists only when there is a
  choice, as an `enum` of the allowed ones, and any other name is
  refused (M21: no model enters through a tool call).
- **Cheaper defaults.** `video_model = "google/veo-3.1-lite"` ($0.08/s
  with audio on OpenRouter's list on 2026-10-21; Veo 3.1 is $0.40/s), and
  `max_videos_per_day = 3` within `max_per_day` (still 20 for all media).
- Files: `ferrule-media` (`ImageOut::cost_usd`, `VideoState::Done`'s
  `cost_usd`, `VideoPrice`, the `video_price`/`speech_price` backend
  calls, the `tree` and `estimate_usd` job columns, added to an older
  table at open), `ferrule-gateway/src/tools/media.rs`, `ferrule-trust`
  `Hub::usd_left`, `ferrule-cli` `config.rs` and `media.rs`.

**Not verified live.** No paid call was made: whether
`/images/generations` (the OpenAI-compatible path ferrule uses, not
`/images`) returns `usage.cost`, and whether a finished video's poll
answer does, are read from OpenRouter's docs, not seen. Without them a
row keeps the estimate (video) or no cost (image), and the counts still
hold.

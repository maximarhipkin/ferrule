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

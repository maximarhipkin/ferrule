# M65 — video it understands, edits and checks before sending

Status: **design, not built** (2026-10-22). Claims M65.

## Why

The owner's words (2026-10-22): when the agent is asked for an image or a
video "with the same person", or a clone, it "creates very bad videos and
sending them and not really checks them and verify". The person comes from
a photo or a video the owner sends in the chat. The owner wants the agent
"skilled with video editing and understanding", and asked for all of it
at once: understanding, editing, the same person, and checking.

What the code does today, and why that happens:

- **`generate_image` sends before the model looks.** It writes the file,
  delivers it to the chat, and only then returns the image to the model
  (`ferrule-gateway/src/tools/media.rs`, `GenerateImageTool::call`). A bad
  picture is already in the chat when the model sees it.
- **A finished video is never seen.** `generate_video` submits a job; the
  built-in `MediaPoller` downloads the clip and delivers it straight to
  the chat ("No model call is ever made here"). No model ever looks at it.
- **"The same person" is a text description.** `VideoReq` has one
  `first_frame` image and nothing else, so a face can't be carried across
  a clip. OpenRouter's video API has `input_references` (reference images
  that guide subject and identity) beside `frame_images`, and ferrule
  doesn't send it.
- **A video sent in the chat is a path.** Telegram (and the other
  adapters) save it to the workspace, and the agent gets a line of text
  saying a video arrived. No frames, no sound. Only photos reach the model
  as pixels (M47).

## Goals

1. **Understanding.** The agent can watch a video: the frames, what is
   said, and the facts (length, size, format, audio or not). This covers
   one the owner sends and one the agent made or edited. On a chat model
   that can't see, photos and tool images are understood too (part 0).
2. **Editing.** Trim, cut, join, crop and resize (9:16, 1:1, 16:9), change
   speed, add subtitles, music or a voice-over (from `speak`), fade, mute,
   extract audio, compress to fit a chat's upload limit.
3. **The same person.** From a photo or a video in the chat, new images
   and videos keep that person's face and look. That includes a talking
   avatar: the person's photo plus a script.
4. **Nothing reaches the chat unchecked.** Every generated image, every
   generated video and every edit is looked at by the model and compared
   with what was asked (and with the person's reference, when there is
   one) before it is sent. A bad result is retried within the caps or
   honestly reported, never sent as if it were fine.

## Non-goals

- Sending video to models as native video input (Gemini's video parts).
  Frames plus a transcript work on every vision model ferrule drives.
- A timeline editor or any dashboard page for editing.
- Real-time or streaming avatars; lip-syncing to arbitrary audio beyond
  what an avatar model on OpenRouter offers.
- New media providers outside OpenRouter.

## Anti-goals (a failure even if it works)

- A generated or edited file reaching the chat before the model has seen
  it. The only exception is the fallback below, and it says it is
  unchecked.
- Retry loops that keep spending: every attempt goes through the existing
  caps, and the model stops and reports after the budget below.
- ffmpeg or ffprobe running outside `ferrule-sandbox` (AGENTS.md: the
  sandbox stays on; new command surfaces go through it).
- Frames, audio or video bytes in the transcript or history. Like M60,
  only paths are kept; pixels go on the wire per request.
- A real person's face recreated for someone who isn't the owner.

## Constraints

- AGENTS.md's hard rules: the sandbox, secrets only through the proxy,
  a truthful status at the end of every run, a byte-stable prompt prefix
  (conditional prompt sections are set per config, like `CHAT_VOICE`, not
  per turn).
- `ferrule_core::vision::MAX_IMAGES` is 4: at most four images go as
  pixels per request, counted across the conversation. Understanding a
  video has to fit in that.
- `media.rs` and the Telegram send branches are M51's surface (PLAN.md,
  "One owner per surface"): make the smallest change and say so.
- The owner's standing rules: no new tests unless asked (existing tests
  are changed to match), and nothing is run live without the owner's
  go-ahead (OpenRouter video calls cost dollars).
- ffmpeg isn't bundled. `ferrule doctor` already reports it (M51.4). With
  no ffmpeg, the video tools say so in one sentence and how to install it.
- Telegram bots can download at most 20 MB (`channels/telegram.rs`, the
  inbox default in `channels/files.rs` is the same). A longer phone clip
  never reaches the workspace; the agent says so and asks for a shorter
  or compressed one. A local Bot API server is out of scope.
- Images go as pixels only under 3.5 MB (`vision.rs`, a byte cap, and an
  image over it is refused, not shrunk). The contact sheet is sized to fit
  under it.

## The design

### 0. A vision helper, for a chat model that can't see

The owner's chat model can't see images (decision 1), so "the agent
looks" means a second model looks and tells it.

- `[media] vision_model`: a cheap image-seeing model on OpenRouter,
  through the same `[media]` backend and the credential proxy. The
  default is a current Gemini Flash; the builder takes the exact id from
  OpenRouter's model list. Empty turns the helper off.
- It is used only where the chat's model doesn't see images, decided
  when the chat's agent is built from its model entry (the same
  `sees_images` the router already uses for photos). A model that sees
  gets pixels as today and never calls the helper.
- **What it does**: one call per image, the image plus a short task,
  answered in text. The text takes the image's place in what the chat
  model reads: `[image <name>, described by <vision_model>: …]`.
  - A photo sent in the chat: described, instead of today's "this model
    can't see photos" note.
  - A tool image (`read_file` on a picture, a browser screenshot,
    `screenshot`, `generate_image`'s result, `watch_video`'s contact
    sheet): described.
  - A check (generated image, generated video, edit): the task names
    what was asked and passes the person's reference image as a second
    image, and asks for a verdict: same person or not, warped faces,
    hands or text, wrong framing, matches the request or not, each with
    what's wrong.
- Each image is described once, cached by its path and modification
  time; it isn't re-sent on later turns.
- Each call is a ledger row (`call_kind = "media"`) and counts against
  the trust hub's dollar caps, like the other media calls.
- Where the description is produced (in the router for photos, in the
  agent's tool-result path for tool images, behind a small trait the CLI
  wires) is left to the builder.

### 1. `watch_video` — the one new tool

`watch_video {path, start?, end?, frames?}` in `ferrule-gateway/src/tools/`,
beside the media tools.

- **Facts** from `ffprobe`: duration, resolution, fps, codecs, size, and
  whether there is audio.
- **One contact sheet**: `frames` frames (default 12, at most 16), spread
  evenly over `start`–`end` (default: the whole clip), tiled into one
  image with ffmpeg's `tile` filter, under the 3.5 MB image cap. One
  image, so it uses one of the four pixel slots. The text result
  lists each tile's timestamp in reading order. To look closer, the model
  calls again with a narrower `start`/`end`.
- **What is said**: the audio track extracted with ffmpeg and sent to the
  gateway's existing `Transcriber` (`transcribe.rs`). Off when there's no
  transcription backend, and then the result says so. At most 10 minutes
  of audio per call unless `start`/`end` narrow it (transcription is paid
  per minute).
- ffmpeg and ffprobe run through `Sandbox::command`, the way
  `ferrule-cli/src/screen.rs` runs its capture helpers: workspace-only
  paths, a timeout, stderr's last line as the error.
- The returned image rides on the tool result through
  `ToolOutput::with_images` (M60).

A chat that receives a video gets one line in its system prompt (a
`CHAT_VIDEO` section, present when ffmpeg is found): watch a video with
`watch_video` before answering about it or editing it.

### 2. Editing — a bundled skill, not a tool

A `video-editing` skill in `crates/ferrule-skills/bundled/`, in the same
format as the six there. It holds ffmpeg recipes the agent runs with the
sandboxed shell: probe, trim (stream copy vs re-encode, and when each is
right), concat (the concat demuxer, and re-encoding mismatched clips),
crop and resize to 9:16, 1:1 and 16:9 (with a blurred pad when the aspect
differs), speed (`setpts` with `atempo`), subtitles (an `.srt` the agent
writes, burned in with the `subtitles` filter where the build has libass,
else attached as a soft track), music under a voice-over with ducking
(`sidechaincompress`), fades, mute, extract audio, a GIF, and compressing
to a size limit (Telegram bots upload at most 50 MB). PowerShell spelling
where it differs. Its last rule: `watch_video` the output, then
`send_file`.

My call: a skill on top of the existing shell rather than an
`edit_video` tool. The shell is already sandboxed and the agent already
uses ffmpeg there. If the eval or real use shows the model fumbling
ffmpeg, a structured tool is the upgrade.

### 3. The same person

- `generate_video` gains `reference: [paths]`: workspace images sent as
  `input_references` (`{"type":"image_url","image_url":{"url":<data URL>}}`)
  beside the existing `first_frame` (`frame_images`). When the source is a
  video, the model picks a clear, front-facing frame from `watch_video`'s
  contact sheet and extracts it at full size with ffmpeg first (the skill
  says how).
- `generate_image` already takes `reference` paths (its edit path). Its
  description gains how to keep a person: pass the photo, say in the
  prompt what must stay the same.
- **Which models can take references** is new work: the backend reads
  `GET /api/v1/videos/models` today only for prices and durations
  (`ferrule-media/src/lib.rs`, `openrouter.rs`). If the listing says per
  model whether it takes `input_references`, that is the source. If it
  doesn't, a `[media] reference_models` list is (my call), defaulting to
  the models below and editable by the owner. Never "send it and see":
  OpenRouter could drop the field silently and bill a video without the
  face. A reference sent to a model that can't take one is refused with
  a sentence naming models that can. On OpenRouter's list (2026-10-22), reference-capable
  ones include `bytedance/seedance-2.5`, `bytedance/seedance-2.0(-fast)`,
  `alibaba/wan-3.0`, `heygen/heygen-video-1` and `heygen/avatar-iv`. Veo
  3.1 takes a first frame and a last frame only.
- **A talking avatar** is `generate_video` with `model =
  "heygen/avatar-iv"` (one photo and a script, with a voice). OpenRouter's
  page names `input_references` and `prompt` but doesn't say where the
  script or an audio track goes. The builder confirms that from
  `GET /api/v1/videos/models` (a free listing) before wiring it. If it
  can't be confirmed, the avatar part is left out and the doc says so.
- **Owner-only** (my call): `generate_video`'s `reference` and the avatar
  model are refused outside the owner's chats and the owner's
  sub-agents, gated at agent construction like the screen tools
  (`main.rs`, the `owners` check). `generate_image`'s `reference` stays
  as it is for everyone it is offered to today: it is also M51's edit
  path ("make this logo rounder"), and code can't tell a face from a
  logo.

### 4. Check before sending

- **Images.** `generate_image` stops delivering on its own. It saves the
  file and returns the image to the model with: not sent yet; look at it
  against the request (and the reference); if it's right, send it with
  `send_file`; if not, try again or say what's wrong. For a chat model
  that can't see, the helper's verdict comes back in the image's place
  (part 0). If the helper is off or fails, the result says the image
  wasn't checked, and `send_file` still needs the model to say why it is
  sending it unchecked (see below).
- **Videos.** When the poller has downloaded a finished clip, it wakes
  the session that asked (`Router::wake`, the path sub-agents use to
  report back) with a message naming the job, the file, the prompt and
  the reference paths, and telling the agent to `watch_video` it, compare
  it, and then `send_file` it or retry or report. The job's `tree` is the
  session id for a chat's own call; the row gains the reference paths
  (a schema change in `ferrule-media/src/jobs.rs`).
  - The job is `checking` after the wake, and `done` when `send_file`
    sends that file. A woken turn can still fail (provider error,
    deadline, `/stop`, a restart: woken turns aren't resumed). So a job
    still `checking` 30 minutes later gets a text note in its chat, not
    the file: the video for "…" finished but wasn't checked, where it is,
    and that "send it" sends it as is. The row is in SQLite, so this
    survives a restart.
  - When the session can't be woken at all (no lane), the same note goes
    at once.
- **Enforced, not only asked for** (my call, because unchecked sending is
  the problem being fixed): `send_file` refuses an image or video file
  this session hasn't looked at since the file last changed. Looked at
  means its pixels, or the helper's description of them (part 0), were
  returned to this session's model by `watch_video`, `read_file` or
  `generate_image`. The refusal says to watch it first. When looking is
  impossible (no ffmpeg, the helper off or failing), `send_file` takes
  an `unchecked` reason, and the file goes with that reason in its
  caption. This also closes the sub-agent gap: a sub-agent's video
  job has no chat, and the parent that later sends the path has to watch
  it. The owner's "send it" after the 30-minute note passes, since the
  owner asked for it unchecked.
- **Edits** are covered by the same `send_file` rule, and by the skill's
  last line (watch, then send).
- **The retry budget**: at most 2 retries per request, then the agent
  reports what's wrong and what it tried. This is a prompt rule backed by
  the existing hard caps (`[media] max_per_day`, video submits per day,
  `max_usd_per_video`, the trust hub's dollar caps). My call: no new
  counter. If the model loops, a per-request attempts counter is the
  upgrade.

### 5. Docs

`docs/models.md` "Making media" (the checks, references, the avatar),
`docs/vision.md` (`watch_video` among the tools that return images),
`docs/roadmap.md` (the M65 line), this file's "As built", and a PLAN.md
Session Log entry per task.

## Done means

- On the Claude plan, a photo sent in the owner's chat is described to
  the agent instead of being a "can't see" note.
- A video sent in the owner's Telegram chat can be described by the agent
  from its frames and transcript.
- No generated image or video reaches a chat before the model has looked
  at it, except the marked fallback.
- `generate_video` carries reference images to a model that takes them,
  and refuses plainly for one that doesn't.
- The talking avatar works, or the doc says why it was left out.
- The `video-editing` skill is bundled and discoverable.
- `make check` is green on all three CI platforms. Nothing is run live
  until the owner says so.

## Left to the builder

The contact sheet's grid, tile size and timestamp layout; the frame-count
default within 8–16; timeouts; where intermediate frames and audio are
kept (inside the workspace's `.ferrule/`) and when they are pruned; how the
poller finds the session id; the prompt and tool-description wording; the
skill's exact recipes; how the transcript's timing is used for subtitles
(today's `Heard` has text only, no segments).

## Decided by the owner (2026-10-22)

1. **The bot chats on the Claude plan**, run through Claude Code. That
   model never sees images (`Entry::sees_images` in
   `ferrule-cli/src/models.rs`), so today it can't see a photo the owner
   sends, a screenshot, or anything it generates. Hence part 0.
2. **Allow the strong models and raise the caps.** Plain videos keep
   `google/veo-3.1-lite` as the default. `[media] video_models` defaults
   to `bytedance/seedance-2.5` (takes references, about $0.10/s),
   `heygen/avatar-iv` (the talking avatar) and `google/veo-3.1-fast`.
   `max_videos_per_day` goes from 3 to 10, `max_usd_per_video` from 1.0
   to 3.0. The setup and doctor text and the docs follow. If
   `heygen/avatar-iv` has no per-second price in the listing, the
   builder prices it from what the listing does give (a per-video SKU),
   because an unpriced model is refused while any dollar cap is set
   (`media.rs`, the estimate check).

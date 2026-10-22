# M65 — video it understands, edits and checks before sending

Status: **built** on `autopilot` (2026-10-22), one commit per section;
**nothing run live** (see "As built" at the end). Claims M65.

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
- A timeline editor, or editing inside the dashboard. The Files page
  (part 5) shows, plays, uploads and deletes; the agent does the editing.
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
- `media.rs` and the Telegram send branches are M51's surface, and the
  dashboard's keys, Safe and config pages are M52's (PLAN.md, "One owner
  per surface"): make the smallest change there and say so. The Files
  page is new; it touches the shared dashboard code only for its routes
  and `limit_for`.
- The owner's standing rules: no new tests unless asked (existing tests
  are changed to match), and nothing is run live without the owner's
  go-ahead (OpenRouter video calls cost dollars).
- ffmpeg isn't bundled. `ferrule doctor` already reports it (M51.4). With
  no ffmpeg, the video tools say so in one sentence and how to install it.
- Telegram bots can download at most 20 MB (`channels/telegram.rs`, the
  inbox default in `channels/files.rs` is the same). A bigger file comes
  in through the dashboard instead (part 5). A local Bot API server is
  out of scope.
- The owner opens the dashboard remotely through a Cloudflare tunnel or
  the relay, and Cloudflare refuses a request body over 100 MB on the
  free plan. The dashboard's server also reads each request whole into
  memory (`dashboard/http.rs`, with a per-route cap in `limit_for`). So a
  big upload goes up in chunks.
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
  the agent made or changed (under `media/`, or written by this session)
  that this session hasn't looked at since the file last changed. Files
  the owner provided (`uploads/`, `inbox/`) pass as they are. Looked at
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

### 5. Big files and a media library, in the dashboard

Asked for by the owner after the first draft: upload big files from the
dashboard for the agent, and a files and media library there.

**Uploading.**

- A **Files** page (`/files`), behind the dashboard's sign-in like every
  page.
- The page cuts the file into chunks of at most 32 MB and sends each with
  `PUT /api/files/upload/<id>?offset=<n>`, with the same checks as
  `/api/chat/photo`: the session, same origin, the CSRF header. Each
  chunk is appended to `<workspace>/uploads/.part/<id>`. A last call
  checks the size and moves the file to `uploads/<day>/<name>`.
  - A dropped connection resumes from the offset the server has.
  - One chunk in memory at a time (the existing `BIG` slot), so
    `http.rs` needs no streaming rewrite.
  - A part file nobody finished is removed after a day.
- Caps: `[dashboard] max_upload_mb` (default 2048). An upload is refused
  when it would leave the disk with less than 1 GB free.
- **From Telegram.** When a file is over Telegram's 20 MB, today's "too
  big" reply (`Inbox::too_big`) gains a button: **Upload it here**, a
  one-time sign-in link to the Files page tied to that chat. When the
  upload finishes, that chat's session is woken (`Router::wake`): the
  file the owner tried to send is at `uploads/…`. The owner's request is
  already in that chat's history, so the agent carries on with it.
- **From the dashboard itself.** After an upload, the page offers a box
  for words. They go with the file to the dashboard's chat as the
  owner's message, the way `say_photo` hands photos over today.

**The library.**

- The same page lists, newest first:
  - what was uploaded (`uploads/`);
  - what chats sent (`inbox/<channel>/<day>/`);
  - what the agent made (`media/`, including `media/screen/`).
- Filters: photos, videos, audio, other; and where it came from.
- Each item shows a thumbnail, its name, size and date, and where it came
  from. Images use the file itself. A video gets a poster frame, made once
  with ffmpeg in the sandbox and cached under `.ferrule/thumbs/`, or no
  thumbnail when there's no ffmpeg. Generated media also show the
  prompt, the model, the cost (from `media_jobs`) and whether the agent
  checked it and sent it.
- Actions:
  - **Open**: images inline, videos and audio in the browser's player;
  - **Download**;
  - **Use in chat**: the dashboard's chat, with the file named in the
    message;
  - **Delete**, behind a confirm. It moves the file to
    `.ferrule/trash/`, and housekeeping empties that after 7 days (my
    call: the owner can take back a slip).
- **Serving files safely.**
  - Only files under those folders, resolved inside the workspace, with
    symlinks refused.
  - `Content-Type` from the extension (`files::mime_for`) with `nosniff`.
  - Anything that isn't an image, video or audio file is served as a
    download (`Content-Disposition: attachment`), so an uploaded HTML or
    SVG file never runs on the dashboard's origin.
  - HTTP `Range` requests are answered, because Safari on iPhone won't
    play a video without them, and seeking needs them everywhere.
  - A file is sent in pieces, never read whole into memory.

**Not in the library** (possible follow-ups): folders and albums, tags,
search beyond the filters, share links for other people, and saved
"people": a named reference photo the agent reuses whenever it's asked
for that person.

### 6. Docs

`docs/models.md` "Making media" (the checks, references, the avatar),
`docs/vision.md` (`watch_video` among the tools that return images),
the dashboard docs (the Files page and its limits),
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
- A 500 MB video uploads from an iPhone through the tunnel, the Telegram
  chat that asked is woken with its path, and the Files page plays it.
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
3. **Big files through the dashboard, and a media library there**
   (asked for after the first draft). Hence part 5.

## As built (2026-10-22)

Built in the design's order on `autopilot`, one commit per section, each
with `make check` green on macOS (1891 passed, 0 failed, 28 ignored, 73
test binaries). No new tests (the owner's rule); the gateway's four media
tests were changed to the new behaviour. Nothing was run live: no
OpenRouter call, no real video through ffmpeg, no upload through a
tunnel. PLAN.md has one Session Log entry per section with the detail.

**§0, the vision helper.** `ferrule_core::vision::Looker`, wired by the
CLI for an agent whose model doesn't see (`gateway_factory`,
`child_builder`); the agent describes images as their message is added
(`push_seen`): the goal's photos, photos sent while it works, every tool
result's images. The line is kept on `ImageRef::described`, so later
turns don't ask again; `ImageRef::check` lets a tool ask for a check
instead. The helper is `ferrule-gateway/src/tools/look.rs` on
`MediaBackend::look` (OpenRouter's `/chat/completions`), a `media`
ledger row charged to the run, answers cached by path, modification time
and task. `[media] vision_model` defaults to `google/gemini-3.8-flash`
(OpenRouter's list, 2026-10-22). The router skips its "can't see photos"
notice when there's a helper; the Claude engine's bridged tool results
carry the lines.

**§1, `watch_video`.** `ferrule-gateway/src/tools/watch.rs`: ffprobe's
facts; one sheet from one ffmpeg run (`-ss` before each input, `trim`,
`scale`+`pad`, `concat`, `tile` with `nb_frames`), 12 frames by default
(1–16) at the middles of equal slices, tiles at most 512 px wide and the
sheet at most 2048 px a side, the times listed in the text (not drawn:
`drawtext` needs fonts a build may lack); the sound as AAC to the
gateway's `Transcriber`, at most 10 minutes a call. ffmpeg and ffprobe
run in the session's sandbox with `-protocol_whitelist file`, a timeout
and stderr's last line as the error; files in `.ferrule/watch/` for a
day. Offered in chats where both programs are found, with `CHAT_VIDEO`.

**§2, editing.** `crates/ferrule-skills/bundled/video-editing/SKILL.md`,
in the bundled pack, with every recipe the design lists and its last rule
(watch, then send; two retries). `speak` gained `send: false` so a
voice-over isn't sent to the chat as a voice message.

**§3, the same person.** `generate_video`'s `reference` (sent as
`input_references`), offered and taken only in the owner's chats and the
owner's sub-agents, refused for a model not in `[media] reference_models`
with the models that take them named. OpenRouter's `/videos/models`
listing (read 2026-10-22) has no field for this, so the list is config,
the design's fallback. The owner's caps and models are the defaults:
`video_models` Seedance 2.5 and Veo 3.1 Fast, 10 videos a day, $3 a
video. Seedance is billed per video token, which the old estimate
couldn't read; ferrule now asks for `resolution: "720p"` and prices a
16:9 720p frame at 24 fps (21,600 tokens a second): Seedance 2.5 about
$0.23/s. That `resolution` is the request field is inferred from the
listing's `supported_resolutions` (the same naming as `aspect_ratio` and
`duration`), not confirmed live.

- **The talking avatar is left out.** The listing gives
  `heygen/avatar-iv` a per-second price ($0.05) and passthrough
  parameters (`voice_id`, `motion_prompt`, …), and its page says the
  speech comes from "a text script … or a supplied audio track", but
  neither names the request field the script or the audio goes in, and
  it lists no frame images. So it isn't in `video_models` or
  `reference_models`. The owner can add it to both to try it with the
  prompt as the script, once a paid test or OpenRouter's docs settle it.

**§4, check before sending.** `generate_image` hands the image back,
not sent, with a check task for the helper; the poller wakes the
session that asked for a finished video (its job is `checking`), and a
video still `checking` after 30 minutes, or whose session can't be
woken, gets the note (`noted`); `media_jobs` gained `refs` and
`woken_at`. `send_file` enforces it through `vision::Looked`, which the
agent fills as tool images reach the model. Two departures:

- The rule covers every image or video except what the owner provided
  (`uploads/`, `inbox/`) and screen captures (`media/screen/`, a faithful
  recording, nothing generated), not "under `media/` or written by this
  session": an edit ffmpeg made in the shell can't be told from any other
  file, so anything not the owner's needs a look.
- `unchecked` is taken only where looking can't be done (no
  `watch_video`, a model that can't see and no helper, or the last look
  failed), or for a video the owner was told about and asked for.

**§5, the Files page.** `crates/ferrule-cli/src/dashboard/files_page.rs`
and a Files section in `app.js`. Departures and choices:

- A `start` call (name, size, the chat's ticket) and a `finish` call
  (the size check, the move) around the design's `PUT`, and a status
  `GET` for resuming; the same file chosen again after a reload resumes.
- The link for a too-big file is in the message's text, not an inline
  button: the login link only ever goes out as the hub's own message, so
  it never reaches a model or a transcript. Any of the owner's chat
  channels gets it, not only Telegram; the ticket lives in memory for a
  day.
- `media_jobs` gained `cost_usd` (an image's bill, a video's when
  fetched) for the library, and `send_file` marks a job `unchecked` when
  it went marked "Not checked".
- `http.rs` gained `Range` answers (206, 416). A `HEAD` on a file is
  answered with its whole body.

**§6, docs.** [models.md](models.md) ("Making media", "Photos"),
[vision.md](vision.md) (`watch_video`, the helper, `Looked`),
[dashboard.md](dashboard.md#files-m65), [skills.md](skills.md),
[housekeeping.md](housekeeping.md), the note at the top of
[m51-media.md](m51-media.md), the README, the roadmap and this section.

**Done means, as it stands.**

| | |
|---|---|
| A photo on the Claude plan is described | built (§0); not run live |
| A video sent in Telegram can be described | built (§1, with §0 for a model that can't see); not run live |
| Nothing generated reaches a chat unseen, except the marked fallback | built and enforced in `send_file` (§4); not run live |
| `generate_video` carries references, and refuses plainly | built (§3); not sent live |
| The talking avatar works, or the doc says why not | left out, said above |
| `video-editing` is bundled and discoverable | built (§2) |
| A 500 MB video from an iPhone, the chat woken, played on the page | built (§5); not run live |
| `make check` green on all three CI platforms | green on macOS at every commit; CI runs when the branch is pushed |


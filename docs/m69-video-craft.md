# M69 — Video craft

The agent makes video, measures it, looks where edits break, has a second
opinion compare versions, and learns the owner's taste. Builds on M65
(`docs/m65-video.md`: `watch_video`, the edit recipes, check before send).

**Status.** Phases 1 and 2 built (below). Phase 3 (`render_video` from an
edit list), phase 4 (hearing the sound through Gemini, motion-graphics
guidance) and phase 5 (the same checks as a plugin for other agents) are
open. Not run live.

## Why

M65's agent watched one sheet of evenly spread frames and an untimed
transcript, and checked nothing it couldn't see. Vision-language models
are poor judges of edit quality from frames: on benchmarks about how
video is put together and about shots, the best models stay near or
under 60% while people are near 90% (VidComposition, arXiv:2411.10979;
ShotBench, arXiv:2506.21356). So quality comes from four layers, strongest
first:

1. **Checks as code.** Numbers that fail a render: clipping, size over the
   send limit, pixel formats phones don't play, mostly-black output.
2. **Looks where edits break.** Frames on both sides of every cut, the
   first and last seconds, one frame at full size for text, each with its
   time on it.
3. **A critic** that compares the new version with the previous one.
   Models judge more reliably comparing two than scoring one
   (arXiv:2402.04788); the skill's critic answers in a fixed shape ending
   in `VERDICT: PASS|FAIL`.
4. **The owner's taste**, from thumbs, comments and example videos, kept
   in files they can read.

## What was built

### `ferrule-video` (new crate)

Runs no process; the gateway runs ffmpeg in the sandbox with what it
builds and passes back what ffmpeg printed. Kept apart so the same checks
can serve other agents (phase 5).

- `detect`: one analysis pass. The picture scaled to 360 high, then
  `blackdetect` (d=0.1, pix_th=0.10), `freezedetect` (n=0.003, d=1) and a
  scene-change `select` above 0.3 with `metadata=print`; the sound through
  `silencedetect` (-45 dB, 0.5 s) and `ebur128=peak=true`. `parse` reads
  stderr. `FILTERS` lists what the pass and the recipes use (`ferrule
  doctor` names the missing ones).
- `check`: `Profile` (`short`, `slides`, `promo`, `long`; guessed from
  shape and length when not given) turns ffprobe's facts and the analysis
  into findings. **FAIL** only for broken output: true peak above
  0 dBTP; size over the chat's send limit (with the bit rate that would
  fit); H.264 not in yuv420p; more than half the video black. **WARN** for
  the rest: loudness more than 2 LU from -14 LUFS; true peak above
  -1 dBTP; silence inside the video longer than 1.5 s (short, promo), 3 s
  (slides) or 5 s (long); black stretches not at the very ends; still
  picture for 3 s (short, promo) or 10 s (long), not checked for slides;
  in a short, one shot of 8 s or more (this milestone's rule of thumb);
  landscape short or upright long; under 720p; no faststart; sound and
  picture lengths differing by more than 0.1 s; no sound track (but
  slides). `captions` lints SRT, WebVTT or ASS: on screen at least 0.35 s
  (short, promo) or 5/6 s (slides, long, after Netflix's English
  subtitle guide); at most 20 characters a second (not applied to social
  chunks of three words or fewer); at most 42 characters a line; no
  overlaps; none after the end. `faststart` reads the file's top-level
  boxes for `moov` before `mdat`. `report` lists findings, then what was
  measured, then what wasn't.
- `labels`: timecodes drawn from a 5×7 digit table into one transparent
  PAM image the size of the sheet, laid over it with `overlay`. No font
  and no `drawtext`: the owner's server's static ffmpeg has neither
  `drawtext` nor fontconfig.
- `transcript`: timed segments from the OpenAI API's `verbose_json`,
  whisper.cpp's `-oj`, a JSON list, SRT or WebVTT.

### Gateway and wiring

- **`check_video {path, profile?, captions?}`**
  (`crates/ferrule-gateway/src/tools/check.rs`): ffprobe, the analysis
  pass (kept per file and modification time, 16 files; its time limit is
  60 s plus 1.5× the length, at most 900 s), faststart, the chat's send
  limit (Telegram 50 MiB, WhatsApp 16 MiB, else `send_file`'s 100 MiB;
  `FileOut::send_cap`), captions. The verdict goes into the session's
  `Looked` (`Looked::checked`), which also counts failing versions per
  folder; at the third the tool says to stop re-rendering.
- **`send_file`**: in a chat where `check_video` is offered, a video the
  agent made goes only once `check_video` measured it as it is now, and
  one with a FAIL only with `known_issues`, which goes into the caption as
  "Known issues: …". Files the owner provided and screen captures pass as
  before (M65 §4), and so does a video the owner asked for as it is.
- **`watch_video`**: `focus` = `overview` (as before; the only look that
  counts for `send_file`), `cuts` (four frames per cut, 0.5 s and 0.1 s
  either side, four cuts a sheet, up to three sheets; cuts come from the
  same kept analysis), `ends` (four frames in each of the first and last
  two seconds), `still` (one frame at full size, at most 2048 px a side).
  Each frame's time is burned on it; if that ffmpeg run fails, the sheet
  is made as before with the times listed in text. With a transcriber
  that gives times, what is said comes as `[0:01.2–0:03.4] words` lines
  (`Heard::segments`).
- **Sub-agents** in the gateway get `watch_video` and `check_video`
  (`child_builder`'s `ChildVideo`), in the chats' video sandbox, narrowed
  for a guest's tree, with the strictest hidden paths. They record nothing
  for `send_file`.
- **Thumbs**: `send_file` sends with `send_with_id` and keeps the last 100
  videos' message ids (`FileOut::thumbed`). The owner's 👍 or 👎 on one
  wakes the session that sent it (`Router::wake`): a 👎 asks once what was
  off and records it, a 👍 notes what worked and ends with `NO_REPLY`.
  This rides on M53.5's Telegram reactions, so it needs `[learning]
  enabled`, and a restart forgets the sent videos.
- **ffmpeg's place**: also found in the workspace's `bin/` (a system
  service's bot can write only its workspace), and `<data>/bin` is on the
  shell's `PATH` (`Sandbox::with_path_dir`). `ferrule doctor` names the
  missing filters and whether Node is there.
- `CHAT_VIDEO` (the chat prompt's video section) says to check before
  sending and to activate the skill for anything beyond a trim.

### The skill

`crates/ferrule-skills/bundled/video-editing/`: `SKILL.md` is the order of
work (brief; plan and storyboard through `ask_user` for bigger jobs; make
in `media/<job>/v1.mp4`, `v2.mp4`, …; `check_video`; the focused looks;
the rubric; the critic; at most three failing versions; send; learn).
`references/`: `rubric.md`, `short-clips.md`, `slides-explainers.md`,
`promos.md`, `long-edits.md`, `sound.md` (-14 LUFS, true peak -1 dBTP,
two-pass `loudnorm` with the target given since its default is -24,
ducking 12–15 dB), `captions-hebrew.md` (fonts through `fontsdir=`, RTL),
`critic-brief.md`, `good-bad-pairs.md`, `taste.md`, `recipes.md` (M65's
recipes).

### Taste

Kept by the agent in the workspace, not in the prompt (the prefix stays
byte-stable): `media/style/profile.md` (rules with where each came from)
and `media/style/references/<name>.md` (one card per example video, with
its measured numbers and the owner's words). The skill reads the profile
at the start of every video job.

## Sources for the numbers

- -14 LUFS for online video, -23 for broadcast; ffmpeg `loudnorm`'s -24
  default: ffmpeg's filter documentation and per-platform loudness
  guides (YouTube turns louder audio down; TikTok and Instagram publish
  no target).
- Caption timing (20 characters a second, 42 a line, 5/6 s minimum):
  Netflix's English Timed Text Style Guide. 0.35 s for social chunks and
  the cut padding (30–200 ms, 30 ms fades): browser-use/video-use's
  editing rules.
- Telegram Bot API: 50 MB per uploaded file.
- Average shot length about 4 s (long-edits guide): Cutting et al., Cornell.
- Murch's order of priorities for a cut (rubric): *In the Blink of an Eye*.
- The safe central area on 1080×1920 (about 900×1400): third-party
  safe-zone guides, not official; the guide says so.
- The 8 s single-shot warning for shorts, the silence and still-picture
  lengths per profile, and the detector settings are this milestone's own
  heuristics.

## Open

- Not run live, and ffmpeg 7.0.2's output on the owner's server is
  unverified; `check_video` and the labelled sheet were run against a
  test clip on macOS (Homebrew ffmpeg) only.
- Phase 3, `render_video {edl}`: an edit list compiled to ffmpeg and run
  as a job that outlives the turn. Until then long renders on the Claude
  engine are limited by its Bash timeout (10 minutes).
- Phase 4: `watch_video {listen}` through the vision helper (Gemini hears
  the track), and motion-graphics guidance once HyperFrames is verified
  on the server.
- Phase 5: the checks and the skill as a plugin other agents install.
- No tests were added for M69 (the owner's choice).

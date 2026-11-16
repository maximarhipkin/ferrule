---
name: video-editing
description: Makes and edits videos well, and checks them before they go. Short vertical clips cut from footage, slides and explainers, promos from generated clips, long landscape edits; trims, cuts, captions, music and voice-over, transitions, compressing to fit a chat. Measures every render (check_video), looks where edits break (watch_video focus cuts), has a critic compare versions, and learns the owner's taste from their thumbs and the example videos they send. Use for any video or audio that has to be made, changed, judged, or learned from.
license: MIT OR Apache-2.0
compatibility: Ships with ferrule. Needs ffmpeg and ffprobe where ferrule runs (`ferrule doctor` says), the shell tool, watch_video and check_video.
---

# Making video well

Your eyes are the weakest check you have: models judge edits poorly from
frames alone. So a video is good when **the numbers pass**, **the risky
moments were looked at**, **a second opinion compared it with the last
version**, and **it follows what the owner has said they like**. Work in
this order every time.

## 1. Brief

Know before you cut: what it is for, who watches it and where (Reels,
Telegram, YouTube), its length, its shape (9:16, 1:1, 16:9), its language,
and what must be in it. Pick the kind, which is also `check_video`'s
`profile`:

| kind | profile | guide |
|---|---|---|
| Vertical clip cut from footage | `short` | `references/short-clips.md` |
| Slides or an explainer made from scratch | `slides` | `references/slides-explainers.md` |
| An ad or promo, often from generated clips | `promo` | `references/promos.md` |
| A longer landscape edit | `long` | `references/long-edits.md` |

Read that guide with `read_skill_file` now.

**Taste first.** If `media/style/profile.md` exists, read it and follow it
over the guides: it is what this owner has said they like. How it is
kept: `references/taste.md`.

## 2. Plan (bigger jobs)

A trim, a mute, a re-crop or a compress: just do it. Anything with a
script, several cuts, slides, music or voice: first send one short plan
with `ask_user` and wait for the OK:

- the beats in order with rough times (hook, body, end), and what is on
  screen and said in each;
- the look (fonts, colours), the sound (music, voice, which voice);
- a storyboard: a contact sheet of the sources or of draft frames, made
  with `watch_video` or ffmpeg, saved under `media/` and sent.

## 3. Make

- `watch_video` every source first. Its transcript has times when the
  transcriber gives them: cut on those, never inside a word.
- Never overwrite an input. Work in `media/<job>/`: `v1.mp4`, `v2.mp4`, …
  so the critic can compare versions.
- More than one cut: write an edit list and `render_video` it
  (`references/edit-list.md`); it fades every cut, ducks the music, burns
  captions last, masters the loudness and runs in the background.
- Recipes for single changes: `references/recipes.md`. Sound: `references/sound.md`.
  Captions (Hebrew included): `references/captions-hebrew.md`.
- No `drawtext`: text goes through `subtitles`/`ass` with `fontsdir=`, or
  an image or HTML clip you overlay.

## 4. Check, look, compare

1. **`check_video {path, profile, captions?}`.** Fix every FAIL before
   anything else; read every WARN and fix it unless the brief wants it
   (a long hold in slides is fine). Check again after each fix.
2. **`watch_video`** the result: `focus: "overview"` (this is the look
   `send_file` counts), then `focus: "cuts"` (where edits break: a flash,
   a jump, a word cut off), `focus: "ends"` (the hook and the ending), and
   `focus: "still"` at any moment with text on screen, to read it.
3. **Score it yourself** against `references/rubric.md`, area by area.
4. **The critic** for anything you planned in step 2: a second opinion that
   didn't make it. `spawn_agent` with role `verifier` and `worktree: false`
   (so it sees `media/`), or the owner's agent `video-critic` if one is
   offered. What to give it and what it answers: `references/critic-brief.md`.
5. Fix what it found, render the next version, and go back to 1.

**At most three failing versions.** `check_video` counts them. After the
third, stop re-rendering: send the best version with `known_issues` saying
what is still wrong, or tell the owner what blocks it. Never say a video is
fine that wasn't checked.

## 5. Send and learn

`send_file` it with a caption that says what it is (and, if you changed
the plan, what changed). A video you made goes only after `check_video` and
the overview look; one still failing goes only with `known_issues`.

The owner's 👍 or 👎 on a sent video, and any example video they send with
"good" or "bad", wake you to learn from it: `references/taste.md`.
Examples of good and bad choices to calibrate against:
`references/good-bad-pairs.md`.

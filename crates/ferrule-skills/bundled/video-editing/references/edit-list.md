# The edit list (`render_video`)

For anything with more than one cut, write the edit as a list and let
`render_video` build it, instead of chaining ffmpeg yourself. It does the
fiddly parts the same way every time: each segment cut on its own and
fitted to one size, 30 ms sound fades at every cut (no clicks), silence
where a clip has none, music ducked under the voice, captions burned in
last, loudness mastered to -14 LUFS, faststart, and the size fitted to
this chat. In a chat it runs in the background and wakes you when it's
done, so long renders don't hit the shell's time limit.

## The fields

```json
{
  "output": "media/promo/v1.mp4",
  "size": [1080, 1920],
  "fps": 30,
  "segments": [
    {"source": "inbox/talk.mp4", "start": 12.40, "end": 15.85},
    {"source": "inbox/talk.mp4", "start": 21.10, "end": 26.30, "zoom": 1.1},
    {"source": "media/promo/slide1.png", "end": 3.0, "push": 0.05},
    {"source": "inbox/broll.mov", "start": 4.0, "end": 7.5, "fit": "blur", "mute": true}
  ],
  "captions": "media/promo/captions.ass",
  "fonts": "media/fonts",
  "voice": {"file": "media/promo/voice.mp3", "at": 0.0, "volume": 1.0},
  "music": {"file": "media/music/bed.mp3", "volume": 0.3, "duck": true},
  "source_volume": 1.0,
  "fade_in": 0.0,
  "fade_out": 0.5
}
```

- `output`: a new `.mp4`. It refuses to overwrite: `v1.mp4`, `v2.mp4`, …
  so the critic can compare versions.
- `size`: 1080×1920 (vertical) when omitted; 1920×1080 for landscape,
  1080×1080 square. `fps`: 30 when omitted.
- `segments`, in order:
  - `source`: a video or an image (`.png`, `.jpg`, `.webp`).
  - `start`, `end`: seconds in the source. Take them from the timed
    transcript and cut in the pauses between words, a little padded
    (30–200 ms into the silence); never inside a word. For an image,
    `end` is how long it shows.
  - `fit`: `crop` (fill, the default), `pad` (whole picture, black
    bars), `blur` (whole picture over a blurred copy, for a landscape
    clip in a vertical video).
  - `zoom`: a fixed zoom into the centre (1.1 every other segment hides
    jump cuts).
  - `push`: a slow push in across the segment (0.03–0.08 keeps a still
    alive).
  - `mute`: leave this clip's own sound out.
- `captions`: an SRT or ASS file timed to the **output** (after the
  cuts). Burned in last. `fonts`: the folder of font files they use; the
  folder's path may hold only letters, digits, `.`, `_`, `-` and `/`.
- `voice`: a voice-over (`speak` with `send: false`), starting `at`
  seconds into the output.
- `music`: under everything, looped to the length, faded out over the
  last 1.5 s; `duck` lowers it under the voice (or under the clips' own
  sound when there is no voice).
- `source_volume`: the clips' own sound (0 for music-only montages).
- `fade_in`, `fade_out`: from and to black, picture and sound.

## Working with it

1. Write the list to `media/<job>/edit.json` with `write_file`, then
   `render_video {edl: "media/<job>/edit.json", dry_run: true}`: it checks
   every source and time and shows the steps without rendering.
2. `render_video {edl: "media/<job>/edit.json"}`. You're woken with the
   result; meanwhile answer other things. `{status: true}` says where it
   is. One render at a time on the machine.
3. Then step 4 of SKILL.md: `check_video`, `watch_video` with focus
   cuts, the critic.
4. For the next version, copy the list, change it, and render to `v2.mp4`.

Not in the list yet: transitions other than fades to black, speed
changes, picture-in-picture, colour grading. Do those to a segment's
source first with ffmpeg (`recipes.md`), save the result under
`media/<job>/`, and use that as the segment's source.

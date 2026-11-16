# Sound

You can't hear. `check_video` measures what you can't: loudness, true
peak, silence, and whether the sound and picture are the same length.
Trust its numbers over your guess.

## Targets

- **-14 LUFS integrated, true peak at most -1 dBTP** for anything online.
  YouTube turns louder videos down; TikTok and Instagram publish no
  official target, and -14 is the usual choice.
- Broadcast (EBU R128) is -23 LUFS; use it only when asked.
- A true peak above 0 dBTP clips: `check_video` fails it.

## Normalise in two passes

ffmpeg's `loudnorm` defaults to -24 LUFS, so always give the target. The
first pass measures, the second applies the measurement:

```sh
# 1. Measure: prints JSON at the end of stderr
ffmpeg -hide_banner -nostdin -i in.mp4 -af loudnorm=I=-14:TP=-1:LRA=11:print_format=json -f null -
# 2. Apply, with the measured_* numbers from step 1
ffmpeg -hide_banner -loglevel error -nostdin -i in.mp4 -af "loudnorm=I=-14:TP=-1:LRA=11:measured_I=<input_i>:measured_TP=<input_tp>:measured_LRA=<input_lra>:measured_thresh=<input_thresh>:offset=<target_offset>:linear=true" -c:v copy -c:a aac -b:a 160k media/<job>/v2.mp4
```

Normalise last, after the mix, once.

## Voice and music

- Duck music 12–15 dB under speech: `sidechaincompress` keyed by the voice
  (the recipe in `recipes.md`), or set music volume per section from the
  timed transcript.
- Music should start on purpose and end on a phrase or fade out over a
  second or two, never cut mid-bar.
- Only music the owner gave you, or that is licensed for this use (CC0
  libraries, the owner's paid service).

## Cuts

- A 30 ms fade in and out of the sound at every cut stops clicks:
  `afade=t=in:d=0.03` and `afade=t=out:st=<len-0.03>:d=0.03` on each
  segment before joining.
- Crossing a cut, sound can lead the picture (J-cut) or trail it (L-cut)
  to make a change of speaker or place feel smooth.

## Clean-up

- Steady background noise: `afftdn` (mild, `nf=-25`) on the voice before
  the mix. Listen-free check: the silence spans in `check_video` should
  stay silence.
- Room levels differ between sources: normalise each source's speech
  before joining, then master the whole.

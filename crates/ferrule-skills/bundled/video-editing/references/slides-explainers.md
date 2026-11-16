# Slides and explainers

`check_video` profile: `slides` (holds are expected; frozen picture isn't
flagged).

## Build

Three ways, simplest first:

1. **Images + ffmpeg.** Render each slide as a 1080×1920 or 1920×1080
   image (`generate_image`, or an HTML page captured by a renderer when one
   is installed), then join them with a slow zoom (`zoompan`) or a
   crossfade (`xfade`) and lay the voice-over under.
2. **libass title cards.** Text slides written as an `.ass` file with
   your fonts (`fontsdir=`), burned over a colour or blurred background.
   No `drawtext` needed.
3. **HTML to video** (HyperFrames or Remotion, when Node and the renderer
   are installed in the workspace): best for animated type and charts.
   Render each animated slide as its own clip; join and mix with ffmpeg.

## Writing slides

- One idea per slide. A headline of a few words, and at most a short line
  under it. If it needs a paragraph, it needs a voice-over instead.
- On screen long enough to read twice: the words on the slide at a
  comfortable reading pace, plus a beat. The voice-over sets the timing:
  change the slide when the voice moves on.
- One or two font families, one size scale (for example 96 / 56 / 36 px on
  1080 wide), one alignment, generous margins (keep within the central
  safe area for vertical).
- Contrast at least 4.5:1 between text and background (3:1 for very large
  text): white on a dark plate, or dark on light.
- Brand colours from the owner's style profile when it has them.

## Motion

- A slow push in (`zoompan` about 1.05× over the slide's time) keeps a
  still alive; never bounce or jolt.
- Transitions: one kind throughout (a short crossfade, about 0.3–0.5 s),
  or plain cuts.
- Reveal points one by one only when the voice reaches each.

## Voice and sound

- Write the script first; it decides the slides. `speak` with `send:
  false` makes the voice; check its length and place slides on its timed
  transcript (`watch_video` of the voice-over muxed on a blank picture).
- Music low under the voice (`sound.md`); silence between sections is
  fine in slides, dead air mid-sentence is not.

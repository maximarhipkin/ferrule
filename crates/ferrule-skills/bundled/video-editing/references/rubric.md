# The rubric

Score each area **pass**, **weak** or **fail**, and for every weak or fail
write the time and what you saw. Every criterion here is something you can
see on a contact sheet, read in `check_video`'s report, or read in the
timed transcript. Areas that don't apply (no captions, no voice) are
skipped, not passed.

What matters most, in Walter Murch's order for choosing a cut: emotion,
then story, then rhythm, then where the eye is, then screen geometry. A
cut that serves the feeling beats a technically perfect one. Use that
order to decide between two fixes.

## Hook (first 3 seconds)

- Something happens or is said in the first second; no logo, black or
  slow fade first (`focus: "ends"`).
- The first frame alone says what this is (it is also the thumbnail).
- Fail: a title card or a silent pause opens a short or a promo.

## Story

- One idea; every beat moves it on. Cut a beat you could remove without
  losing anything.
- It ends on purpose: a payoff, a call to action, a last line. It doesn't
  just stop (`focus: "ends"`).

## Pacing and rhythm

- Shot lengths match the kind (the guide for it). `check_video` gives the
  cuts, the median shot and the longest.
- A hold is long because something is happening in it, not because the
  edit ran out of material.
- Cutting to music: cuts land on beats where the music is the point.

## Cuts

On `focus: "cuts"`, each row is one cut, before and after:

- No flash frame, no black frame, no repeated or skipped frame across it.
- A cut on speech: no word cut in half (check against the timed
  transcript), and the next line doesn't start mid-breath.
- Matching cuts: the subject's position or movement carries across, or
  the change is big enough to read as deliberate. A small jump of the
  same framing looks like a mistake.

## Captions and on-screen text

- `check_video` with `captions` passes: every caption readable in time,
  lines short enough, none overlapping.
- On `focus: "still"`: every word legible at phone size; strong contrast
  (outline or box on busy picture); nothing in the platform's covered
  margins (keep text in the central area, see the guide).
- Text matches what is said, spelled right; Hebrew reads right to left
  with punctuation in place (`captions-hebrew.md`).

## Slides and graphics

- One idea per slide; it can be read in the time it is up.
- Consistent type: one or two families, one size scale, one alignment.
- Motion has a purpose (reveal, point, transition) and eases in and out;
  nothing moves just to move.

## Effects and transitions

- Plain cuts by default. A transition says something (time passing, a
  change of place); the same one throughout.
- Zooms and pans on stills are slow and steady, never a jolt.

## Sound

- `check_video`: no FAIL on peaks; loudness near -14 LUFS; no unwanted
  silence; sound and picture the same length.
- Voice is always clear over music: music is ducked under speech
  (`sound.md`).
- No click or pop at a cut (30 ms fades); music starts and ends on
  purpose, not cut mid-phrase.

## Colour and look

- Shots that sit together match in brightness and colour temperature.
- Nothing is crushed black or blown white where detail matters (faces,
  products, text).
- Generated clips: no warped hands or faces, no garbled text, the same
  person stays the same person.

## Delivery

- `check_video` passes on codec, pixel format, faststart and size.
- The right shape for where it goes (9:16 for Reels/Shorts/TikTok, 16:9
  for YouTube).

## The verdict

Ship when no area is **fail** and the owner's style profile is followed.
When two versions are both fine, compare them side by side and say which
is better and why; don't give scores out of ten (they drift and flatter).

# Captions and text, Hebrew included

## Fonts without fontconfig

Some servers' ffmpeg has no fontconfig and no `drawtext`. libass then
finds a font only where you point it:

- Keep fonts in `media/fonts/` (`.ttf`/`.otf`). For Hebrew, a family that
  covers it (Heebo, Assistant, Rubik, Noto Sans Hebrew). Check with
  `list_dir` that the file is there; if none is, say so and ask, or fetch
  one under its licence (Google Fonts families are OFL).
- Burn with `subtitles=<file>:fontsdir=media/fonts:force_style='FontName=<family>,…'`,
  or an `.ass` file whose style names the family. The `FontName` is the
  family name inside the file, not the file name.

## Hebrew right to left

- Write the lines in logical order (as typed). libass with fribidi lays
  them out right to left; don't reverse them yourself.
- Punctuation and numbers at the line's ends can jump to the wrong side.
  Look at every caption with `watch_video {focus: "still", start: <time>}`
  and fix: wrapping the line in RLE/PDF marks (U+202B … U+202C) or moving
  the punctuation usually does it.
- Mixed Hebrew and English in one line: keep the English short; check it.
- Line breaks: break between phrases, never inside a construct pair
  (בית הספר) or a name; two short lines beat one long one.

## Timing and size

- `check_video` with `captions` checks every caption is up long enough,
  not too fast to read, lines not too long, none overlapping.
- Social chunks (one to four words) for shorts; full sentences, at most
  two lines, for long edits and explainers.
- Large enough to read on a phone: on 1080 wide, around 60–80 px for
  shorts, a thick outline or a box behind it.
- Inside the safe area (the guide for the kind).

## An .ass skeleton

Write it with `write_file`; times are `h:mm:ss.cc`.

```
[Script Info]
ScriptType: v4.00+
PlayResX: 1080
PlayResY: 1920

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Main,Heebo,72,&H00FFFFFF,&H00000000,&H64000000,1,0,0,0,100,100,0,0,1,5,0,2,90,90,520,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
Dialogue: 0,0:00:00.50,0:00:01.40,Main,,0,0,0,,שלום לכולם
```

`Alignment` 2 is bottom centre; `MarginV` 520 lifts it above the apps'
covered bottom on 1920 high.

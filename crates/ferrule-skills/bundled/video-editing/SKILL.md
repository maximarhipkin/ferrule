---
name: video-editing
description: Edits video and audio with ffmpeg in the sandboxed shell. Trim, cut, join, crop and resize to 9:16, 1:1 or 16:9, change speed, add subtitles, music or a voice-over, fade, mute, take the sound out, make a GIF, take a still frame, and compress a video to fit a chat's upload limit. Use whenever a video or an audio file has to be changed or made from others.
license: MIT OR Apache-2.0
compatibility: Ships with ferrule. Needs ffmpeg and ffprobe on the machine ferrule runs on (`ferrule doctor` says), and the shell tool.
---

# Editing video with ffmpeg

You run ffmpeg yourself with the shell tool; it runs in the sandbox, which
can write only inside the workspace. These rules hold for every recipe:

- **Watch first.** `watch_video` the input before you change it: its
  length, size, frame rate and sound, and what is in it. The recipes need
  those numbers.
- **Never overwrite the input.** Write each result to a new file under
  `media/` (`media/edit-trimmed.mp4`, `media/edit-9x16.mp4`, …).
- **Start every command with** `ffmpeg -hide_banner -loglevel error -nostdin`
  (shortened to `ffmpeg …` below) so the output is only errors.
- **Encode for every player** (Telegram's, an iPhone's) when you
  re-encode. Below, `<h264>` stands for these options; write them out in
  its place:

  ```
  -c:v libx264 -preset veryfast -crf 23 -pix_fmt yuv420p -movflags +faststart -c:a aac -b:a 128k
  ```

  An ffmpeg without libx264 (`ffmpeg -hide_banner -encoders` doesn't list
  it) uses `-c:v h264_videotoolbox -b:v 6M` on a Mac, else `-c:v mpeg4
  -q:v 4`.
- **Long jobs.** The shell stops a command after 2 minutes. An encode of
  a long or large video runs with the shell's `background: true`, and you
  follow it with `shell_job`.
- **Text files** (a concat list, subtitles) are written with `write_file`,
  not with the shell: it works the same on every system.
- **Last, always:** `watch_video` the result, compare it with what was
  asked, and only then `send_file` it. If it's wrong, fix it and watch
  again; after two attempts that don't come out right, say what is wrong
  and what you tried instead of sending it.

## Trim

Two ways, and when each is right:

```sh
# Fast and lossless, no re-encode: cuts land on the nearest keyframes, so
# the start can be a second or two early. Right for rough cuts.
ffmpeg … -ss 00:00:05 -to 00:00:12 -i in.mp4 -c copy -avoid_negative_ts make_zero media/edit-trim.mp4

# Exact to the frame, re-encoded. Right when the cut points matter, or when
# a filter runs anyway.
ffmpeg … -ss 5 -to 12 -i in.mp4 <h264> media/edit-trim.mp4
```

Times are seconds (`12.5`) or `hh:mm:ss.ms`. `-ss` and `-to` before `-i`
seek fast and count from the start of the file.

## Cut a part out

Trim the parts to keep, then join them (below). With stream copy, cut
both parts from the same file so they match.

## Join

Clips with the same codec, size, frame rate and sound (parts of one file,
or renders from one model at one size): the concat demuxer, no
re-encode. Write `list.txt` with `write_file`:

```
file 'part1.mp4'
file 'part2.mp4'
```

```sh
ffmpeg … -f concat -i list.txt -c copy media/edit-joined.mp4
```

Clips that differ (sizes, frame rates, phone and generated video): the
concat filter, which re-encodes. Bring each to one size, frame rate and
sample rate first:

```sh
ffmpeg … -i a.mp4 -i b.mp4 -filter_complex "[0:v]scale=1080:1920:force_original_aspect_ratio=decrease,pad=1080:1920:(ow-iw)/2:(oh-ih)/2,setsar=1,fps=30[v0];[1:v]scale=1080:1920:force_original_aspect_ratio=decrease,pad=1080:1920:(ow-iw)/2:(oh-ih)/2,setsar=1,fps=30[v1];[0:a]aresample=48000[a0];[1:a]aresample=48000[a1];[v0][a0][v1][a1]concat=n=2:v=1:a=1[v][a]" -map "[v]" -map "[a]" <h264> media/edit-joined.mp4
```

Every clip needs sound for `a=1`. Give a silent clip silence first:

```sh
ffmpeg … -i b.mp4 -f lavfi -i anullsrc=r=48000:cl=stereo -shortest -c:v copy -c:a aac b-sound.mp4
```

## Crop and resize to 9:16, 1:1 or 16:9

Fill the frame, cropping the sides or the top and bottom (centred):

```sh
# 9:16, 1080×1920 (Stories, Reels, Shorts, TikTok)
ffmpeg … -i in.mp4 -vf "crop='min(iw,ih*9/16)':'min(ih,iw*16/9)',scale=1080:1920,setsar=1" <h264> media/edit-9x16.mp4
# 1:1, 1080×1080
ffmpeg … -i in.mp4 -vf "crop='min(iw,ih)':'min(iw,ih)',scale=1080:1080,setsar=1" <h264> media/edit-1x1.mp4
# 16:9, 1920×1080
ffmpeg … -i in.mp4 -vf "crop='min(iw,ih*16/9)':'min(ih,iw*9/16)',scale=1920:1080,setsar=1" <h264> media/edit-16x9.mp4
```

Keep the whole picture instead, on a blurred copy of itself (when the
crop would cut off what matters, a face or a caption):

```sh
ffmpeg … -i in.mp4 -filter_complex "[0:v]scale=1080:1920:force_original_aspect_ratio=increase,crop=1080:1920,boxblur=20:2[bg];[0:v]scale=1080:1920:force_original_aspect_ratio=decrease[fg];[bg][fg]overlay=(W-w)/2:(H-h)/2,setsar=1[v]" -map "[v]" -map "0:a?" <h264> media/edit-9x16.mp4
```

For 1:1 or 16:9, put `1080:1080` or `1920:1080` where it says
`1080:1920`. `0:a?` keeps the sound when there is some.

## Speed

```sh
# Twice as fast
ffmpeg … -i in.mp4 -filter_complex "[0:v]setpts=PTS/2[v];[0:a]atempo=2.0[a]" -map "[v]" -map "[a]" <h264> media/edit-fast.mp4
# Half speed (slow motion)
ffmpeg … -i in.mp4 -filter_complex "[0:v]setpts=PTS*2[v];[0:a]atempo=0.5[a]" -map "[v]" -map "[a]" <h264> media/edit-slow.mp4
```

`setpts=PTS/x` and `atempo=x` go together. An older ffmpeg takes
`atempo` only from 0.5 to 2.0: chain it (`atempo=2.0,atempo=2.0` is 4×).
No sound: drop the `atempo` part and use `-vf "setpts=PTS/2" -an`.

## Subtitles

Write the lines as an `.srt` with `write_file`:

```
1
00:00:00,500 --> 00:00:03,200
First line.

2
00:00:03,400 --> 00:00:06,000
Second line.
```

`watch_video` gives what is said but not when each sentence starts; to
time the lines, watch narrower parts (`start`, `end`) and note where each
sentence falls.

Burned into the picture (shows everywhere, Telegram's player included).
It needs an ffmpeg built with libass: `ffmpeg -hide_banner -filters`
lists `subtitles`.

```sh
ffmpeg … -i in.mp4 -vf "subtitles=subs.srt:force_style='FontSize=22,Outline=2,MarginV=40'" <h264> media/edit-subtitled.mp4
```

Without libass, attach them as a track a player can turn on (not shown by
Telegram's inline player; say so):

```sh
ffmpeg … -i in.mp4 -i subs.srt -map 0 -map 1 -c copy -c:s mov_text media/edit-subtitled.mp4
```

Keep the `.srt`'s name plain (letters, digits, `-`, `_`): the filter's
argument is parsed, and other characters need escaping.

## A voice-over

Make the voice with `speak` and `send: false` (it saves an MP3 under
`media/` and doesn't send it). Then put it under the picture:

```sh
# The voice replaces the clip's own sound
ffmpeg … -i in.mp4 -i media/voice.mp3 -map 0:v -map 1:a -c:v copy -c:a aac -b:a 160k -shortest media/edit-voiced.mp4
# The voice over the clip's own sound, turned down
ffmpeg … -i in.mp4 -i media/voice.mp3 -filter_complex "[0:a]volume=0.25[bed];[bed][1:a]amix=inputs=2:duration=first:normalize=0[a]" -map 0:v -map "[a]" -c:v copy -c:a aac -b:a 160k media/edit-voiced.mp4
```

`-shortest` ends with the shorter of the two; leave it out to keep the
whole picture.

## Music under a voice-over, ducked

The music drops while the voice speaks (`sidechaincompress`), loops for
as long as the picture runs, and stops with it:

```sh
ffmpeg … -i in.mp4 -i media/voice.mp3 -stream_loop -1 -i music.mp3 -filter_complex "[1:a]asplit=2[vo][key];[2:a]volume=0.5[m];[m][key]sidechaincompress=threshold=0.03:ratio=8:attack=20:release=400[duck];[vo][duck]amix=inputs=2:duration=longest:normalize=0[a]" -map 0:v -map "[a]" -c:v copy -c:a aac -b:a 160k -shortest media/edit-music.mp4
```

Music alone, quieter: `-filter_complex "[1:a]volume=0.4[a]"` with
`-map 0:v -map "[a]"`. An ffmpeg older than 4.4 has no `normalize` on
`amix`: drop `:normalize=0` and add `,volume=2` after `amix=…`. Only use
music the owner gave you or has the right to use.

## Fade in and out

With the length `L` (seconds) from `watch_video`, half a second each way:

```sh
ffmpeg … -i in.mp4 -vf "fade=t=in:st=0:d=0.5,fade=t=out:st=L-0.5:d=0.5" -af "afade=t=in:st=0:d=0.5,afade=t=out:st=L-0.5:d=0.5" <h264> media/edit-faded.mp4
```

Write the number for `L-0.5` (`11.5` for a 12-second clip).

## Mute, or take the sound out

```sh
# Mute: the picture alone, no re-encode
ffmpeg … -i in.mp4 -c:v copy -an media/edit-muted.mp4
# The sound alone, as MP3 (or `-c:a aac media/sound.m4a` without libmp3lame)
ffmpeg … -i in.mp4 -vn -c:a libmp3lame -q:a 2 media/sound.mp3
```

## A GIF

```sh
ffmpeg … -ss 2 -t 4 -i in.mp4 -filter_complex "fps=12,scale=480:-1:flags=lanczos,split[a][b];[a]palettegen[p];[b][p]paletteuse" -loop 0 media/edit.gif
```

Keep it short and small: a GIF is many times the size of the same MP4.

## A still frame at full size

For a photo of a moment, or a person's reference image for
`generate_video` and `generate_image`: find the moment on `watch_video`'s
sheet (for a person, a tile where the face is clear, front-facing and
well lit), narrow `start`/`end` around it to choose the exact time, then:

```sh
ffmpeg … -ss 12.4 -i in.mp4 -frames:v 1 -q:v 2 media/frame-12.4.jpg
```

Look at the frame with `read_file` before you use it.

## Compress to fit a size

Telegram bots upload at most 50 MB; aim a little under the limit. The
video's bit rate in kb/s is the size in megabytes × 8,000 ÷ the length in
seconds, less the sound's (96 kb/s), less 5% for the container. For 45 MB
and 300 s: 45 × 8000 ÷ 300 = 1200, less 96, less 5% ≈ 1040k. Two passes
hit the size; scaling down to 720p helps a lot:

```sh
ffmpeg … -y -i in.mp4 -vf "scale=-2:'min(720,ih)'" -c:v libx264 -preset veryfast -b:v 1040k -pass 1 -an -f mp4 /dev/null
ffmpeg … -i in.mp4 -vf "scale=-2:'min(720,ih)'" -c:v libx264 -preset veryfast -b:v 1040k -pass 2 -pix_fmt yuv420p -movflags +faststart -c:a aac -b:a 96k media/edit-small.mp4
```

Then delete the `ffmpeg2pass-0.log*` files the first pass left. Check the
result's size before you send it; still too big: lower the rate and run
again.

## PowerShell (Windows)

The commands are the same; only the shell differs:

- `ffmpeg` is `ffmpeg.exe` when it isn't on `PATH` by that name.
- `/dev/null` is `NUL`.
- A line is continued with a backtick (`` ` ``), not `\`.
- Keep filtergraphs in double quotes and the quotes inside them single,
  as above.

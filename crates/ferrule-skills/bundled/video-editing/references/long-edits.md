# Long landscape edits (YouTube-style)

`check_video` profile: `long`.

## Shape and size

- 1920×1080 (16:9), the source frame rate (don't convert 25 to 30), H.264
  + AAC, yuv420p, faststart.
- A long video rarely fits a chat: Telegram bots send at most 50 MB.
  `check_video` says when it is too big and the bit rate that would fit;
  for a long edit, offer a smaller preview and say where the full file is.
- Renders take long on a small server: render in parts (a section each),
  check each, then join with the concat demuxer (no re-encode).

## Work from the transcript

1. `watch_video` each source: the timed transcript is your map.
2. Write the edit as a list before cutting: for each kept range, the
   source, start and end (on word boundaries), and why it stays. Remove
   repeats, false starts and tangents.
3. Build it, then watch the cuts.

## Pacing

- Hollywood films average about 4 s a shot; talking-head videos hold far
  longer. Change the picture when the topic changes: B-roll, a screen
  recording, a different angle, a chapter title.
- `check_video` warns at 10 s of still picture in a long edit: look at
  each one and keep it only if it earns its time.

## Structure

- Say what the video gives in the first 30 seconds.
- Chapters: a title card or a lower third at each section, and the times
  in the caption.
- End with a summary or the next step, not a fade into nothing.

## Sound

- Speech is the priority: consistent level across sources (normalise each
  source, then master to -14 LUFS), room noise reduced (`sound.md`).
- Music only under intros, transitions and B-roll, low under speech.

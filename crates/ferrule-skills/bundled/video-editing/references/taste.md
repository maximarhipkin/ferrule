# Learning the owner's taste

The owner's taste beats every guide here. It lives in the workspace, in
plain files they can read and change:

- `media/style/profile.md`: the rules, short, one per line, newest
  wins when two conflict. Group them: pacing, captions, type and colour,
  sound, music, transitions, length, things to avoid. Each rule says where
  it came from (`👎 on media/promo/v3.mp4, 2026-11-20: "too fast"`).
- `media/style/references/<name>.md`: one card per example video.

Read `profile.md` at the start of every video job (SKILL.md step 1).

## An example video ("this is good", "I like this", "not like this")

1. `check_video` it (pick the profile it looks like) and `watch_video` it
   (overview, cuts, ends).
2. Write a card `media/style/references/<short-name>.md`:
   - liked or disliked, the owner's own words, the date;
   - the measured numbers: length, shape, cuts a minute, median and
     longest shot, loudness, whether it has captions and music;
   - what you see: caption style (size, position, chunking, font look),
     colours, transitions, how it opens and ends, the voice.
3. Turn what is clearly intended into rules in `profile.md` ("captions:
   two to three words, bold, centred, yellow on the key word"). One
   example is a hint; mark a rule from a single example as tentative.
4. Tell the owner in one or two lines what you took from it.

## A 👎 on a video you sent

You are woken with the file's name. Ask once, in one short message, what
was off: pace, captions, sound, the look, or something else. When they
answer, add a rule to `profile.md` with where it came from, and offer a
fixed version. Don't ask again about the same video.

## A 👍 on a video you sent

You are woken with the file's name. Write a short card for it in
`media/style/references/` (marked "liked, from a 👍") with its measured
numbers from `check_video`, and confirm in `profile.md` any tentative rule
it follows. Don't reply: end with NO_REPLY.

## Comments

Anything the owner says about a video ("the music is too loud", "I want
bigger subtitles") is a rule: write it to `profile.md` the same way, even
if they didn't ask you to remember it.

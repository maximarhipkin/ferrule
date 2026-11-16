# Promos and ads (often from generated clips)

`check_video` profile: `promo`.

## Plan the shots before generating

Generated clips cost money and minutes each. Write the shot list first
(step 2 of SKILL.md) and get the OK: for each shot, what is seen, the
camera (static, slow push, orbit), the length, and how it connects to the
next. Then generate only those.

- Keep one look across shots: the same subject description, the same
  light and colour words, the same aspect ratio, and a reference image for
  any person or product (`generate_video` with `reference`).
- Generate a little longer than needed and trim to the best part.
- Watch every clip (`watch_video`, then `focus: "still"` on faces, hands
  and any text): warped hands, melting faces, garbled lettering and a
  person who changes between shots are the usual faults. Regenerate or
  cut around them; never ship them.

## Structure

1. Hook: the product or the payoff in the first seconds.
2. Two or three beats: the problem, the product in use, the result.
3. The offer and the call to action, held long enough to read, with the
   brand.

## Edit

- Cut on motion or on the music's beat; vary shot sizes (wide, medium,
  close).
- Text: a few words per card, brand fonts and colours (style profile),
  inside the safe area. Generated video is bad at lettering: put all text
  on in the edit, never in the prompt.
- Sound: music carries a promo; a voice-over or a line of captioned copy
  says the offer. Master to -14 LUFS.

## Check

`check_video` with `profile: "promo"`, the cuts sheet, and the critic, as
in SKILL.md. Say in the caption which clips are generated when the owner
should know.

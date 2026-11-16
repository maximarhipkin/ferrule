# The critic

A second opinion from an agent that didn't make the video. Models judge
better when they compare two versions than when they score one, so the
critic always compares, when there is a previous version.

## Starting it

`spawn_agent` with role `verifier` and `worktree: false` (it must see the
files under `media/`), or `agent: "video-critic"` when the owner defined
one (it may run on another model). Give it a task like this, filled in:

```
You are reviewing a video edit. Don't fix anything; judge it.

Brief: <what it is for, where it goes, length, shape, language>.
Kind: <short | slides | promo | long>.
New version: media/<job>/v3.mp4. Previous version: media/<job>/v2.mp4
(or: none, this is the first).
Owner's style: <the rules from media/style/profile.md that apply, or "none yet">.

1. Run check_video on the new version (profile <kind>) and on the
   previous one.
2. watch_video both with focus "overview", then the new one with focus
   "cuts" and "ends", and focus "still" where text is on screen.
3. Read the rubric with read_skill_file: video-editing,
   references/rubric.md. Judge the new version area by area.

Answer in exactly this shape:
- One line per problem: `<time> | <area> | <what is wrong> | <fix>`,
  most serious first. Only problems you can point to on a sheet, in the
  report or in the transcript.
- `Compared with the previous version: better | worse | same`, and the
  one change that made the difference.
- Last line: `VERDICT: PASS` if nothing is fail by the rubric and the
  owner's style is followed, else `VERDICT: FAIL`.
```

## Using its answer

- Fix the problems in order, render the next version, check again.
- If it says **worse** than the previous version, go back to the
  previous one and make a smaller change.
- It can be wrong. A finding that contradicts `check_video`'s numbers or
  the owner's own words loses; say so when you send.
- Its PASS doesn't replace yours: send only when `check_video` passes too.

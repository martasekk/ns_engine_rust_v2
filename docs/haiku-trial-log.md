# Haiku trial log — flow editor into v2

Every implementation step on branch `flows` is done by a Haiku subagent (`worker`, `model: haiku`). The lead
(Opus) writes the spec (`docs/flows-spec.md`) and each brief, re-runs the gate itself, and reviews every diff.

## What counts as a mistake

| code | meaning |
|---|---|
| GATE | the agent reported done, but the gate (fmt / clippy / tests) fails when the lead re-runs it |
| SCOPE | touched a file outside the brief, or ran `cargo fmt` over the workspace |
| SPEC | the code does something other than the spec or brief says (names, shapes, behaviour) |
| CLAIM | the report claims something that is not true (tests pass, file written, …) |
| BUG | a defect the lead found in review that the gate did not catch |
| TEST | a required test missing, or one that would pass without the change |

One line per mistake. A fix run is a new attempt on the same chunk.

## Tally

| chunk | attempts | mistakes |
|---|---|---|
| 1 flow types | 1 | 0 |
| 2 validate / offer / note | 1 | 2 (minor) |
| 3 engine narrows tools, adds note | 1 | 0 |
| 4 flow files on disk | 1 | 0 |
| 5 `[library] flow` → startup → engine | 1 | 1 (minor) |
| 6 save-time report | 1 | 0 |
| 7 endpoints + routes | 1 | 0 |
| 8 editor page + link | 1 | 1 (minor) |
| 9 editor layout fixes | 1 | 0 |
| 10 slot keys + remember-unlocks test | 1 | 1 (minor) |
| **total** | **10 runs, 0 re-runs** | **5, all minor (1 CLAIM, 4 PROCESS); 0 bugs, 0 gate failures, 0 out-of-scope edits** |

Chunk 10 closed two gaps the lead found after the first finished version: the engine tests seeded the fact
directly, so nothing proved that a `remember_fact` under the key the note names unlocks the flow; and a flow could
name a slot `remember_fact` refuses as a key. Both were gaps in the lead's spec, not agent mistakes.

PROCESS = an instruction about how to work (e.g. tests first) not followed, with no effect on the result.

## Not Haiku's mistakes (the lead's or the old code's)

Found in the lead's browser check of the finished page, and fixed in chunk 9. Every one was in the brief or the
copied v1 page, and every agent did what it was told:

- v1's layout: the top bar and the inspector panel are both pinned 12px from the top, so on a window narrower than
  about 1100px an open inspector covers Save. The v1 fix in MERGE_INSTRUCTIONS was checked with the panel closed.
- Lead's brief 8: the "← Settings" link got no colour, so it rendered dark blue on the dark panel.
- Lead's brief 8: missed the Save button's v1 tooltip ("takes effect on the next turn …"), which is false in v2.
- Lead's own slip: the first launch.json used `bash`, which the preview runner resolves to a different shell.

## Mistakes

| chunk | attempt | code | what | caught by | fixed by |
|---|---|---|---|---|---|
| 2 | 1 | CLAIM | report said 17 new tests; there are 15 (every case the brief listed is covered) | lead counted `#[test]` | not needed |
| 2 | 1 | PROCESS | wrote code and tests together, not tests first (self-reported) | agent's own report | not needed |
| 5 | 1 | PROCESS | asked to check after rustfmt that it changed no lines but its own; did not inspect each hunk (self-reported). The lead read the diff: clean | agent's own report | not needed |
| 10 | 1 | PROCESS | wrote Part A's test and code in one pass, never saw the test fail first (self-reported) | agent's own report | not needed |
| 8 | 1 | PROCESS | asked to map each of the 13 diff hunks to the brief's items a-h; did not (self-reported). The lead mapped them: all 13 were correct | agent's own report | not needed |

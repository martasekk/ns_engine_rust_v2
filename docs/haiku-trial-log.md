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

## Mistakes

| chunk | attempt | code | what | caught by | fixed by |
|---|---|---|---|---|---|
</content>
</invoke>

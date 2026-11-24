# Plans: what is open (index, 24 Nov 2026)

Every plan, spec, design note and step list for this repo lives in `docs/plans/` as `YYYY-MM-DD-<topic>.md`.
This file is the index: one row per plan that still has open items. Add the row when you write a plan; update it
the same day a step lands. When a plan's last item closes, or a newer plan replaces it, `git mv` it to
`docs/plans/archive/` and drop its row. A partly stale plan is rewritten in place, current state first. Never two
live plans for one topic: extend the existing one.

## Open

| Plan | What it is | State | Left |
|---|---|---|---|
| `2026-11-24-rules-rebuild.md` | Brings the code in line with the coding rules (structure, contract first, determinism, property/Miri/loom tests) and adds the project `CLAUDE.md` (step RR-0) | Planned and reviewed 24 Nov, fixes applied (`2026-11-24-rules-rebuild-review.md`); nothing started | All steps, gate zero (RR-G0) first; 8 open questions for the user (Q0-Q7); RR-14b may need a re-plan; see the plan's "How to run it" |

## Not in this folder yet

These live outside this branch; whoever owns them moves them here (and adds a row) when they land on main:
- `docs/superpowers/plans/2026-11-18-prompt-caching.md`, untracked in the main checkout (branch `prompt-cache-phase1`).
- The flow-engine merge plan described in `MERGE_INSTRUCTIONS.txt` (untracked in the main checkout).

## Archive

`archive/` holds the September plans and design specs (m1-m12, provider, pointer, multi-conversation and
multi-tenant runtime, settings vault; neuro-symbolic, evolution, memory and engine-structure designs), moved from
`docs/superpowers/` on 24 Nov 2026. Their checkboxes were never ticked, so their state was not verified item by
item: the code has moved on since. If one still has an open item, write it into a current plan with a row here
instead of reviving the old file.

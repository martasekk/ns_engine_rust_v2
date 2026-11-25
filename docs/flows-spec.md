# Flows in ns-engine v2 — spec

Brings the flow editor of the old flow engine (`C:\Users\Acemagic\repos\ns_engine_rust`, "v1") into v2,
keeping v2's structure. A flow is a script that narrows which registered tools the emitter is offered and
tells it which details it still has to collect. Written 2026-11-23 by the lead; implemented by Haiku agents
one chunk at a time (log: `docs/haiku-trial-log.md`).

## Decisions

- **Format.** v1's flow JSON, unchanged, so every v1 flow file loads and the v1 editor works on it. Same
  field names, same serde attributes. Fields v2 does not act on yet (`fast_path`, `templates`,
  `reply_template`, `follow_up_ms`, an action's `reads`/`writes`/`args`/`write_scope`) are kept, validated
  where v1 validated them, and reported as warnings at save time ("stored, not run by v2 yet").
- **Where flows live.** Files, like the rest of v2's library (`personas/`, `modules/`):
  `flows/<id>/<version>.json` under the config root. Every save is a new version file; a save must bump
  the version (v1's rule); deleting the highest version is the rollback; the last version cannot be deleted.
- **Which flow a company runs.** `[library] flow = "<id>"` in the company overlay (or the base config),
  resolved like `[library] persona`: the highest version of `flows/<id>/` is loaded and validated at
  startup. A missing or invalid flow stops startup with the company named. Restart to pick up an edit, as
  for personas.
- **What a flow does in a turn** (engine). Before the loop, once per turn, the engine reads the fact keys
  in the turn's scope (`memory.facts(&scope, "")`). A role is *filled* when the fact key its slot names
  exists. Then:
  - registered tools are offered only if some flow action whose guard holds has `component == tool name`;
    a tool the flow does not name is not offered. Engine-owned actions (ask_clarification, remember_fact,
    recall, …) are never narrowed by a flow.
  - if required roles are missing, one guidance line goes first in the emitter's guidance, naming each
    missing role and the fact key to remember it under.
  - `IntentIs` never holds in v2 (v2 has no intent classifier); a flow using it gets a save-time warning.
  - Held fixed for the turn, like `scope_holds_facts`. A fact remembered mid-turn counts from the next
    turn. Store error: fail open (no narrowing, no note) — same rule as the existing store questions.
- **Editor.** v1's `admin_ui.html` (working-tree copy, which has the `[hidden]` fix) becomes
  `app/src/admin/flows.html`, served at `GET /flows` by `ns-app admin`, with v1's six endpoints under
  `/api/flows…` and v2's per-run token. The settings page gets a "Flows" link that carries the token.

## Types (crate `nscore`, file `crates/core/src/flow.rs`)

```rust
#[serde(tag = "when", content = "value", rename_all = "snake_case")]
pub enum Guard { Always, Never, RolesFilled(Vec<String>), RoleMissing(String), IntentIs(String) }
#[serde(rename_all = "snake_case")]
pub enum WriteScope { Turn, Flow, Conversation, User }
pub struct ActionDecl { name: String, component: String, reads: Vec<String>, writes: Vec<String>,
                        write_scope: WriteScope, args: BTreeMap<String, String>, when: Guard }
pub struct FastPathRule { match_on: Option<String>, pattern: Option<String>,
                          sets: BTreeMap<String, serde_json::Value>, reply_template: String }
pub struct FlowDef { id: String, version: u32, roles: BTreeMap<String, String>, required_roles: Vec<String>,
                     actions: Vec<ActionDecl>, fast_path: Vec<FastPathRule>, reply_template: Option<String>,
                     #[serde(default)] templates: BTreeMap<String, String>,
                     #[serde(default)] follow_up_ms: Option<i64> }
pub struct FlowOffer { pub tools: BTreeSet<String>, pub missing: Vec<String> }
```
All derive `Clone, PartialEq, Eq, Debug, Serialize, Deserialize` (FlowOffer: `Clone, PartialEq, Eq, Debug,
Default`), all fields `pub`. Role and slot names are plain `String`s (v1 used `#[serde(transparent)]`
newtypes, so the JSON is identical).

## Endpoints (`ns-app admin`, all but the page need `Authorization: Bearer <token>`)

| method | path | body | 200 response | errors |
|---|---|---|---|---|
| GET | `/flows` | – | the editor page (no auth, like `/`) | |
| GET | `/api/flows` | – | `{"flows":[{"id","latest_version"}]}` sorted by id | |
| GET | `/api/flows/<id>` (`?version=N` optional) | – | the flow JSON | 404 `{"error"}` |
| GET | `/api/flows/<id>/versions` | – | `{"versions":[1,2,…]}` ascending; `[]` for unknown | |
| POST | `/api/flows/<id>` | flow JSON | `{"saved":{"id","version"},"warnings":[…],"note":"…"}` | 422 `{"errors","warnings"}` invalid or id mismatch; 409 `{"error","latest_version"}` no bump |
| DELETE | `/api/flows/<id>/versions/<N>` | – | `{"deleted":{"id","version"},"latest_version":M}` | 404 unknown; 409 last version |
| POST | `/api/validate-flow` | flow JSON | `{"ok","errors","warnings"}` | |

`<id>` must pass `valid_library_name` (tenant.rs); otherwise 400. (Validate is not under `/api/flows/` so
that no flow id is reserved.)

## Chunks

| # | what | files |
|---|---|---|
| 1 | flow types + serde + v1 fixture round-trip | `crates/core/src/flow.rs`, `crates/core/src/lib.rs`, `crates/core/tests/flow.rs`, fixtures |
| 2 | `validate`, `guard_holds`, `offer`, `note` | `crates/core/src/flow.rs`, `crates/core/tests/flow.rs` |
| 3 | engine: `EngineConfig.flow`, narrowing, note | `crates/engine/src/turn/{config,run,legal,prompt}.rs`, `crates/engine/tests/flow.rs`, every `EngineConfig {` literal |
| 4 | flow files on disk | `app/src/flowlib.rs`, `app/src/main.rs` (mod line) |
| 5 | `[library] flow` → startup → engine | `app/src/config.rs`, `app/src/tenant.rs`, `app/src/factory.rs` |
| 6 | `check_flow` save-time report | `app/src/admin/flows.rs` |
| 7 | endpoints + routes | `app/src/admin/flows.rs`, `app/src/admin/mod.rs` |
| 8 | editor page + link | `app/src/admin/flows.html`, `app/src/admin/page.html`, `app/src/admin/mod.rs` |

## Rules for every chunk

- Work only in `C:\Users\Acemagic\ns_engine_rust_v2-flows` (branch `flows`). Touch only the files the brief names.
- **Never run `cargo fmt`** (write mode): 13 files are unformatted at the branch point and must stay as they are.
  Format a file you created with `rustfmt --edition 2021 <file>`; in an existing file, format your own lines by hand.
- Build with `--target-dir C:/Users/Acemagic/cargo-targets/flows -j 8`.
- Gate: `bash C:/Users/Acemagic/ns_engine_rust_v2-flows/docs/flows-gate.sh "<-p crate …>" <cargo test args>`,
  e.g. `bash docs/flows-gate.sh "-p ns-core" --test flow`. Last line PASS or FAIL. Do not commit; the lead commits.
- Never run `rustfmt` on a `lib.rs`, `main.rs` or `mod.rs`: it follows `mod` lines into the unformatted files.

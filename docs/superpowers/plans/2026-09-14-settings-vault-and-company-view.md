# The settings page grows up — a vault, a company list, and shared parts

Date: 2026-09-14 · Builds on `2026-09-13-multi-tenant-runtime.md` (phases 1–6
built) and the `ns-app admin` page that landed with it.

**Target.** Setting a company up is done in a form and nowhere else: pick or
mint its keys from a vault, give it its own model, its own persona and its own
set of modules, and say which of those its users may reach. Nothing typed
twice, nothing typed into a file.

---

## 0. What exists already, so the next session does not rebuild it

`ns-app admin` (loopback, one printed token per run) serves three tabs from
`app/src/admin/`:

| file | what it owns |
|---|---|
| `mod.rs` | the socket, the routes, the bearer check, `/api/state` |
| `schema.rs` | `PROCESS` and `COMPANY`: the allowlist of dotted paths, one `Field` each |
| `document.rs` | `toml_edit` read/set/get/write — comments survive, every save must parse as an `AppConfig` first |
| `secrets.rs` | `.env` beside the config; values never returned to the browser, only set/unset and source |
| `page.html` | the whole page, compiled in with `include_str!` |

Two rules the page keeps, and this plan does not get to bend either:

- **A secret never enters the config.** The config names a variable; the value
  lives in `.env` (or was exported, which wins). That is what keeps a key out
  of a file somebody commits.
- **A secret never leaves for the browser.** The page is told a name, what
  references it, and whether it is set. `no_secret_value_is_ever_in_what_the_page_receives`
  is the test that says so, and it must still pass at the end of this.

**What is already per company** (`tenant::PROCESS_OWNED` is the whole list of
what is not): `[persona]`, `[store]`, `[auth]`, `[whatsapp]`, `[memory]`,
`[router]`, `[[http_component]]`, `[engine] worker_slots`, and — the one that
matters for "its own model" — **all of `[llm]`**. The overlay layer already
supports every per-company thing this plan asks for except the library.

**What is process-owned and stays so:** `[http]`, `[models]`, `serve.listen`,
`serve.token_env`, `serve.auth`, `serve.hello_timeout_ms`,
`engine.shard_worker_slots`.

---

## 1. Decisions, with the alternatives that lost

**D1. The vault is the set of named variables, not a new store.** A vault
entry *is* an environment variable name plus a value in `.env`. Picking a key
for a company sets that company's `*_env` field to the entry's name.

*Rejected: a vault with its own ids, and the config referring to vault ids.*
It would put a second naming layer between the config and the value, and the
config would stop being readable on its own — which is the property that lets
an operator who has never opened this page run the thing from a shell.

**D2. A shared key is shown as shared, never silently.** Two companies naming
one entry share a provider quota and — by `throttle_for`'s key of
(base URL, key hash) — one throttle: A's traffic paces B. That is sometimes
right and never something to discover later, so every vault row names every
company that references it.

**D3. A company cannot have its own listening port.** One socket belongs to
the process however many companies arrive on it. Companies are separated by
credential (the token's `iss`) and, on a webhook, by the business account id
*inside the signed payload* — never by anything the caller chose, which is why
`/hooks/<platform>/<company>` is not a route this will ever add.

*What a company can have* is its own account on a platform, which it already
does. "Its own webhook" in the UI therefore means its own credentials and its
own business number, on the shard's one endpoint.

**D4. Personas and modules are a library of files, referenced by name.**
`personas/<name>.md` and `modules/<name>.toml` beside `tenants/`. A company
names what it uses: `persona = "support-brief"`, `modules = ["orders", "stock"]`.

*Rejected: copying the text into each overlay.* Two companies would drift
apart silently, which is the thing "shared" is supposed to prevent.

*Rejected: a database.* Files until they hurt — a file is reviewable and
diffable, which is the same answer the tenant overlays got.

**D5. An inline value beats a reference; modules are a union.** A company that
writes its own `[persona] text` gets it, and the reference is ignored with the
override reported. Modules add rather than replace, because "this company also
has X" is the normal case and "this company has none of the shared ones" is
not.

**D6. Groups are the naming layer over per-session tool grants, and nothing
is enforced in this plan.** The multi-tenant plan's Phase 8 already owns the
enforcement — "per-session tool grants from the token's `grants` claim, and
the first consumer of `trust`" — and names the hard part: tools are registered
once at assembly, so a per-session subset needs a filter the turn loop does
not have. Groups are stored, shown, and labelled *not yet enforced*.

*Rejected: inventing a second mechanism now.* Two ways to say who may reach a
tool is one more than anybody can hold in their head.

---

## 2. Rules for every phase

- Target `x86_64-pc-windows-gnu`; `cargo` lives in `~/.cargo/bin` and is not
  on the Git Bash PATH. `cargo test --workspace` green at the end of every
  phase; write the output to a file and read the counts from it.
- `rustfmt` **only on the files actually changed** — `cargo fmt --all`
  reformats untouched crates and the diff stops being readable.
- Every save still passes `document::loads_as_config` before it is written,
  and every new field is on the `schema.rs` allowlist or it cannot be written
  at all.
- The page is one file. If it passes ~1,200 lines, split it before adding to
  it, not after.
- After code changes: `graphify <root> --update`.

---

## 3. Hazards, each with a reproduction test

| id | hazard | fixed in |
|---|---|---|
| H1 | Two companies pick one key and share a quota and a throttle without either page saying so | 1 |
| H2 | A secret value reaches the browser as the vault grows | 1 |
| H3 | A company is renamed; its store file, its `*_env` names and its session ids do not follow | 2 |
| H4 | A shared persona is edited for one company and changes every company using it | 3 |
| H5 | A company references a persona or module that is not there, and starts anyway | 3 |
| H6 | Groups are stored but unenforced, and the page reads as though they are enforced | 4 |

---

## Phase 1 — the vault

**T1.1 `/api/vault`.** Every entry: name, value set or not, where from
(`environment` beats `file`), and **every config path that references it** —
built by extending `schema::variables_named`, which already walks the config
rather than the environment. A variable nothing references is listed as
*unreferenced* rather than hidden, so a key added before the company that will
use it is visible.

**T1.2 Add, replace, remove.** Already in `secrets.rs`; the vault is a view
over it. Removing an entry two companies reference is refused, naming both.

**T1.3 H2: the test grows with the feature.** Extend
`no_secret_value_is_ever_in_what_the_page_receives` to the vault route.

**T1.4 H1: sharing is visible.** `a_key_two_companies_reference_says_so`: the
row names both, and the page prints the consequence in words — one quota, one
throttle.

**Exit.** Green; the Credentials tab is a vault; no value in any response.

## Phase 2 — companies as a list, and a company's own model

**T2.1 The list.** Name, model, which ways in it is reachable by, and the
count of its variables that are missing — which is the one number that says
"this company will not work yet".

**T2.2 Its own model.** Add `llm.provider`, `llm.base_url`, `llm.api_key_env`
and `llm.emitter.model` to `schema::COMPANY`. The config layer already
supports it; only the page did not offer it. `[models]` stays process-owned.

**T2.3 Key fields become vault pickers.** A `Kind::EnvName` control renders as
a dropdown over vault entries plus *new…*, which mints
`NS_<PURPOSE>_<COMPANY>` and asks for the value once.

**T2.4 H3: no rename.** A company's id is the first segment of every session
id it has ever had and the name of its store file. Renaming is refused with
the reason; copying is not offered either.

**T2.5** `no_company_field_is_one_the_loader_would_refuse` still passes — it
walks `COMPANY` against `PROCESS_OWNED` and is what stops this phase adding a
control whose every save is refused.

**Exit.** Green; a company is added, given a model and a key, and is reachable
without touching a file.

## Phase 3 — the library: personas and modules

**T3.1 Two directories**, `personas/` and `modules/`, loaded beside the tenant
set. A persona is prose; a module is what `[[http_component]]` already is.

**T3.2 Reference and override.** `persona = "<name>"` and
`modules = ["<name>", …]` in an overlay. Inline persona wins (D5); modules are
a union.

**T3.3 H5: a missing reference is refused by name at load**, beside the
existing overlay refusals, with the file it should have been.

**T3.4 H4: editing a shared thing says who else it changes.** The page names
every company using it *before* the save, not after.

**T3.5 Tests.** `a_company_can_use_a_shared_persona_and_override_it`;
`modules_from_the_library_and_the_overlay_are_one_set`;
`a_missing_persona_is_refused_naming_the_file`;
`editing_a_shared_module_reports_every_company_it_reaches`.

**Exit.** Green; two companies share one persona, one of them overrides it,
and both say so on the page.

## Phase 4 — groups, designed and inert

**T4.1 The shape.** Per company: a group is a name and a set of module names.
Stored in the overlay, shown in the company view.

**T4.2 H6: labelled.** Every group control says, in the page, that nothing is
enforced yet and which plan owns the enforcement (multi-tenant Phase 8).

**T4.3 The seam, written down and not built.** A group reaches a session
through the token's `grants` claim; enforcement is a filter over the
registered tool set inside the turn loop. Record it here; do not add a second
mechanism.

**Exit.** Green; groups can be expressed and cannot yet do anything, and the
page does not pretend otherwise.

---

## Sequencing

1 blocks 2 (the picker needs the vault). 3 is independent of both and is the
biggest single piece. 4 is small and wants 3, because a group names modules.

A defensible first cut: **1 and 2**. That is the whole "add a company, give it
a key and a model, without opening a file" story, and it is most of the value.

## Open questions

1. Is the WhatsApp verification token per process or per company? It is
   `[http] whatsapp_verify_token_env` today, which is the endpoint's. Meta
   subscribes per app; if two companies bring their own apps, it is theirs.
   Proposed answer: leave it process-owned until a second Meta app exists.
2. Does a module carry its own secret (an API key for the endpoint it calls)?
   `[[http_component]]` has no key field today. If it grows one, it names a
   vault entry like everything else.
3. Should the vault ever show a value? No. If somebody needs to read a key
   back, they have `.env` and a shell, and that is the right place for it.

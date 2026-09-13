# Many tenants at once — implementation plan

Date: 2026-09-13 · Builds on `2026-09-10-multi-conversation-runtime.md` (phases 0–3
built and committed) and its findings doc, plus the 2026-09-13 research on how Stream,
Intercom, Ably, LiveKit and Temporal place identity and isolation.

**Target.** ~100 companies on one product. Each has many end users chatting at once,
its own persona, tools, models, budget and data, and reaches the engine over more than
one channel (a web widget, WhatsApp). No company's data, behaviour, spend or failure
may reach another's.

---

## 0. What this adds, and what it deliberately does not

The 2026-09-10 work made *sessions* concurrent, in-process, with Temporal's two
properties: per-execution serialization (a mailbox and a task per session) and worker
slots (a semaphore across them). The engine's own event log stays the only history.

This plan adds the level above — *tenants* — and the level below the engine — *identity
at the edge*. It does not revisit durability. A crash still loses each session's
in-flight turn, decided in findings §2.7 on a safety argument: a `PendingConfirmation`
made durable before it was shown lets the next message confirm something nobody asked
about. Serving a hundred companies does not change that argument.

---

## 1. Decisions, with the alternatives that lost

**D1. The instance boundary is the tenant, not the chat.** One `Engine` per company,
many sessions inside it, exactly one `Dispatcher` per `Engine`.

*Rejected: an engine per chat.* A chat already gets its own task and mailbox, which is
the right granularity for ordering one conversation. An engine per chat would give each
user of one company a separate store handle, tool registry and model client, and would
put company-shared things — standing knowledge, budget, learned rules, the consolidator
— in the wrong place, or in a hundred places.

**D2. The factory is an extraction, not a design.** Everything that varies per company
is already a field of `EngineConfig` or a slot of `HarnessParts`. The factory is the
function from one tenant's config to one `Arc<Engine>`. That function exists today,
inlined in `app/src/main.rs::main` (from `main.rs:539`, ~540 lines). Phase 1 lifts it
out with no behaviour change.

**D3. Two levels of placement, so 100 tenants is not 100 processes.** A *shard* is a
process hosting many tenants; inside it a `TenantRegistry` builds a tenant on first use
and evicts it when idle. Routing to a shard is sticky by tenant.

*Rejected: one process per tenant.* Clean, and correct at ten tenants. At a hundred it
is a hundred configs, a hundred deployments and a hundred idle engines holding memory
for companies whose users are asleep. The registry gives the same isolation boundary in
code and lets a shard hold only the tenants currently talking.

*Rejected: one engine for all tenants with a tenant key threaded through.* This is the
design that leaks. Every render path, every store query and every tool call would have
to carry and honour the key, and one missed call site is a cross-company disclosure.

**D4. One store file per tenant.** `ns-<tenant>.sqlite`, opened by the factory.
Isolation stops being a property of a mapping function and becomes a property of the
filesystem. It also makes the `Mutex<Connection>` contention measured last round
per-tenant rather than global, and makes "this company left, delete their data" one
`rm`.

**D5. The fact scope inside a tenant is the end user.** A turn renders exactly one
scope, so per-user standing facts and company-wide standing facts cannot both appear as
facts. Per-user is the privacy-safe default. Company knowledge belongs in the persona,
the tools and retrieval. Written down because this is the decision most likely to be
made by accident, in a one-line closure, by someone who has not read this file.

**D6. Identity is resolved at the edge, and `Incoming` is not touched.** Each channel
provides one resolver from raw arrival to a verified `(tenant, session, trust)` triple;
everything downstream reads only its output. The derived `SessionId` carries tenant,
channel and an opaque subject, so nothing further needs a new field.

*Rejected: adding fields to `Incoming`.* It is `{ session, text }` and is constructed
in about 120 places across the workspace, most of them tests. Trust level has exactly
one consumer today (per-session tool grants, which are gated to Phase 8), so paying
that cost now buys nothing.

**D7. A failed turn is a failed session.** Only a store failure ends a shard. This is
optional today and mandatory once one process serves twenty companies.

---

## 2. Rules for every phase

- Target `x86_64-pc-windows-gnu`; `cargo` lives in `~/.cargo/bin` and is not on the Git
  Bash PATH. `cargo test --workspace` is green at the end of every phase; write the full
  output to a file and read the counts from it, never pipe through `tail`.
- `rustfmt` only on the files actually changed (it follows `mod` trees).
- Implementers do not commit; the reviewer does, one commit per phase.
- Every behavioural claim here has a named test. A phase whose tests do not exist is not
  done.
- **The CLI stays byte-identical after every phase.** `ns-app` with no tenant config is
  one tenant called `local`, one slot, the `global` fact scope, failures fatal. Each
  phase states how it holds this.
- Replay (`replay.rs`, `verify_patch`) produces the same events for the same recording
  before and after. Nothing here changes what a turn *does*.
- After code changes, rebuild the graph: `graphify <root> --update`.

---

## 3. Hazards, each with a reproduction test

Named here so they are written once and cited by the phase that fixes them.

| id | hazard | fixed in |
|---|---|---|
| H1 | Two messages for a cold tenant build two engines over one store file | 3 |
| H2 | A turn error ends the process, taking every other tenant with it | 2 |
| H3 | A client names its own session id, so any valid token impersonates and displaces any user | 4 |
| H4 | A webhook retry runs the turn twice: two replies, two bills, two logged turns | 6 |
| H5 | A tenant evicted on idle loses a message that lands mid-teardown | 3 |
| H6 | A full mailbox blocks the accept loop, stalling every other tenant | 3 |
| H7 | Two browser tabs on one session displace each other in a loop | 5 |
| H8 | Two tenants sharing a provider URL share one throttle, so A's traffic paces B | 1 |
| H9 | Two tenant overlays naming the same store path silently merge two companies | 1 |
| H10 | One process-wide wire trace interleaves every tenant's prompts into one file | 1 |

H8 and H10 are not guesses. `throttle_for` (`main.rs:208`) memoizes throttles in a
process-global map keyed by base URL alone, and `trace_sink` (`main.rs:224`) is a
process-global `OnceLock` over one path from `NS_TRACE`.

---

## Phase 0 — pin and measure

Nothing ships. Two numbers and three tests decide the shape of everything after.

**T0.1 What one idle tenant costs.** A scratch cargo project *outside the repo*
depending on this checkout by path. Build N engines from one config against N scratch
SQLite files; report resident memory and wall-clock per engine for N = 1, 10, 50.
Nothing lands in the repo but the numbers in §Results. This number, not taste, sets
tenants per shard in D3.

**T0.2 Two engines share nothing.**
`two_engines_on_separate_stores_do_not_see_each_others_events` in
`crates/engine/tests/turn_loop.rs`: two engines over two `InMemoryStore`s, one turn each
under the *same* session id; each store holds only its own turn, `verify_chain` passes
on both, and neither fact table holds the other's key.

**T0.3 H1 reproduced.** `two_messages_for_a_cold_tenant_build_one_engine`, written
against the registry API Phase 3 will provide, `#[ignore]`d until it exists. It is the
guard, not a discovery.

**T0.4 H2 reproduced.** `a_failing_turn_ends_the_whole_dispatcher`: a scripted emitter
errors on session `a`; assert today's behaviour — `run` returns `Err` and session `b`
never completes its queued turn. Phase 2 inverts it and keeps this one as the
documentation of what the classification prevents, the way T0.3 of the previous plan
was kept.

**Exit.** Workspace green; §Results has T0.1; T0.2 and T0.4 pass; T0.3 compiles.

---

## Phase 1 — the factory, and the three process-global leaks

**Why.** Nothing else can start until an engine can be built from a tenant's config, and
until the three things that are process-global stop being so.

**T1.1 Extract.** `app/src/factory.rs`:
`build_engine(cfg: &TenantConfig, mode: Mode) -> Result<Arc<Engine>, StartupError>`,
lifted from the body of `main`. `main` calls it for the CLI and for `serve`. The diff
should read as a move: no logic changes, and every `process::exit` inside the moved code
becomes a `StartupError` variant so a bad tenant refuses itself instead of killing a
shard that hosts nineteen good ones.

**T1.2 Tenant config layering.** `TenantConfig` is the base `config.toml` plus an
overlay from `tenants/<id>.toml`. Overlayable: `[persona]`, `[templates]`, `[llm]`
targets, `[memory]`, `[router]`, `[http_component]`, `[engine] worker_slots`,
`max_requests`, store path, signing keys, channel credentials. Not overlayable: what the
process owns — listen address, trace configuration, `[models]`. Refuse a missing or
malformed overlay by name at load. Assembly is already a gate (`HarnessBuilder::build`
refuses a missing slot, a duplicate slot, two tools claiming one name); this keeps it
one.

**T1.3 H9: store paths are unique.** Loading the tenant set refuses two overlays that
resolve to the same store path, naming both tenants. A copy-pasted overlay is the
likeliest way two companies' data ever ends up in one file, and it would be silent.

**T1.4 H8: the throttle is per credential, not per URL.** Key the throttle registry by
base URL *and* a hash of the API key, never the key itself. Two tenants on the same
provider with their own keys get their own pacing; two tenants sharing a key still share
pacing, which is correct, because they share a quota.

**T1.5 H10: the wire trace is per tenant or off.** In serve mode, `NS_TRACE` either
names a directory that gets one file per tenant, or is refused. One file holding every
company's prompts is a disclosure waiting for the first operator who opens it.

**T1.6 Tests.** `factory_builds_two_tenants_with_different_personas_and_tools`;
`a_malformed_tenant_overlay_is_refused_by_name`;
`two_overlays_sharing_a_store_path_are_refused_naming_both`;
`two_tenants_with_distinct_keys_get_distinct_throttles`;
`two_tenants_sharing_a_key_share_a_throttle`;
`serve_mode_refuses_a_trace_path_that_is_a_regular_file`;
`cli_mode_still_accepts_a_trace_file_path`. Existing CLI and serve end-to-end tests
unchanged.

**CLI identical:** with no `tenants/` directory the factory builds exactly one tenant
from `config.toml` and every default is today's default.

**Exit.** Green; no `Engine` is constructed outside the factory; no throttle, trace or
store is shared across tenants by construction.

---

## Phase 2 — failure isolation

**Why.** H2. `session_task` does `engine.run_turn(incoming).await?`, and that question
mark is the entire failure policy; `Dispatcher::reap` turns any task error into an `Err`
out of `run`, and a panicking task is re-raised with `resume_unwind`.

**Corrected by Phase 0 (T0.4, 2026-09-13).** The blast radius is narrower than this
plan first assumed, and the correction matters because it removes a branch rather than
adding one. `EngineError` has exactly three variants — `Store`, `Channel`, `RequestCap`
(`crates/engine/src/turn/mod.rs:72`) — and a model failure is not among them: an emitter
or replier error is absorbed inside `run_turn` and answered with the fallback reply
(`fallback_reply_explains_provider_error`, `turn_loop.rs:2143`). So a provider outage
already degrades to a sentence rather than a dead process, and the "anything else" arm
originally written here has nothing to catch. Any future plan text saying "the emitter
errors, so the turn fails" is wrong.

**T2.1 Classify at the call site.** A `TurnFailure` policy on the dispatcher, `Fatal`
for the CLI and `Isolate` for serve. Three variants, three answers:

- `EngineError::Store` — fatal for the shard under both policies. The disk is gone.
- `EngineError::RequestCap` — ends this *tenant's* dispatcher, never the shard. It is a
  deliberate stop for a metered run, not a fault.
- `EngineError::Channel` — under `Isolate`, ends that session and its connection, and
  nothing else. This is the whole of H2 in practice: one tenant's widget disappearing
  mid-reply currently ends a process serving every other tenant.

The match is exhaustive with no wildcard arm, so a fourth variant added later cannot
inherit a policy by accident — it has to be classified deliberately, and the compiler
will insist.

**T2.2 Contain panics at the session task boundary.** `catch_unwind` around the turn
under `Isolate`; report it as a failed session. Safe for a reason worth recording: the
store's lock is a `tokio::sync::Mutex`, which does not poison the way `std`'s does, and
`run_turn` takes `&self`, so a panic leaves no unusable lock and little in-memory state
to corrupt. Under `Fatal` the panic is still re-raised, so the CLI is unchanged.

**T2.3 A failed turn leaves a loadable log.** A turn that fails partway has already
appended events. The next turn on that session must fold and verify cleanly. Appends are
idempotent by event id, so this is expected to hold — which is exactly why it needs a
test that fails loudly if someone changes `append`.

**T2.4 Tests.** T0.4 inverted, using the same injector it established — a channel `send`
that fails for session `a` alone: `a_failing_turn_ends_its_session_not_the_dispatcher`,
where `a` fails and `b` completes two further turns. `a_store_error_still_ends_the_run`.
`a_request_cap_ends_one_tenant_and_not_the_shard`.
`a_panicking_turn_does_not_take_the_shard_down`.
`a_session_whose_turn_failed_accepts_the_next_message_and_verifies`.
`the_cli_policy_still_ends_the_process_on_a_turn_error`.

**Exit.** Green; under `Isolate` the only errors that leave a `Dispatcher` are a store
failure and a spent request cap. *(Corrected during implementation: an earlier draft of
this line said "a store failure" alone, which contradicts T2.1's own text. A spent cap
must end that tenant's dispatcher rather than let its other sessions keep spending, so
it leaves as an `Err` too. `a_request_cap_ends_one_tenant_and_not_the_shard` pins the
reading.)*

---

## Phase 3 — the tenant registry and the split channel

The hard phase. Today a `Dispatcher` owns the channel and its read loop; many tenants in
one process need one listener.

**T3.1 `TenantChannel`.** Implements `Channel`: `recv` awaits this tenant's inbound
`mpsc::Receiver` behind interior mutability — which the trait already requires, since
both methods take `&self` — and `send` routes by session id through the shard's shared
connection map. One per tenant, handed to that tenant's `Dispatcher` as
`Arc<dyn Channel>`. **Nothing inside `Dispatcher` changes.** Note the build order this
forces: `TenantChannel` exists before `HarnessParts`, because `build` refuses a missing
channel slot.

*Phase 1 left one line for this phase to remove.* `Engine::run` consumes `self`
(`turn/run.rs:709`) while the factory returns `Arc<Engine>`, the shape the registry
wants, so `main` currently unwraps the `Arc` with an `expect` (`main.rs:465`). It holds
today because the factory hands back the only handle, and it becomes a latent panic the
moment anything clones that handle — which is exactly what a registry does. Phase 3
deletes it by constructing the `Dispatcher` from the `Arc` directly, which is already
its constructor's signature, rather than going through `Engine::run`.

**T3.2 `TenantRegistry`.** `resolve(&self, tenant) -> Result<Arc<TenantHandle>, _>`
returns the live handle or builds one *under a per-tenant build lock* (H1): factory,
`TenantChannel`, `Dispatcher`, spawned task. A tenant idle for `tenant_idle_after` is
evicted with the same generation-and-leftovers discipline the session mailboxes use, so
a message landing during teardown is re-delivered rather than lost (H5). That discipline
was got wrong once already and corrected in the last plan's "Phase 2, as built"; copy it
rather than reinvent it.

**T3.3 The listener stops reading the socket, not the accept loop.** Per connection:
authenticate (Phase 4), resolve the tenant, push into that tenant's queue.
Backpressure blocks *that* connection's read, never the accept loop and never another
tenant's delivery (H6). This is the residual the previous plan left in `WithDesktop`,
fixed here rather than inherited.

**T3.4 Two-level slots.** A tenant's `worker_slots` is its fairness bound. A shard-wide
semaphore acquired *after* the tenant permit bounds total in-flight model calls. Fixed
acquisition order, so no deadlock. Document the order in the type, not in a comment
somewhere else.

**T3.5 Per-tenant observability.** Turn counts, failures, in-flight turns and spend per
tenant, readable without a debugger. Usage attribution is already per turn (previous
plan, Phase 1); this aggregates it per tenant so a bill can be produced and a runaway
tenant can be seen.

**T3.6 Tests.** `two_tenants_run_turns_concurrently` (tenant A's turn parks on a
`Notify` only B's completion releases; passes only if they overlap);
`one_tenants_flood_does_not_delay_another` (A fills its queue; B completes within the
test timeout — H6); `two_messages_for_a_cold_tenant_build_one_engine` (T0.3
un-`#[ignore]`d — H1); `an_evicted_tenant_is_rebuilt_on_its_next_message`;
`a_message_landing_during_tenant_teardown_is_not_lost` (H5);
`a_tenants_request_cap_does_not_stop_another_tenant`.

**T3.0 `worker_slots` needs a serve default, and it is not 1.** Added 2026-09-13 while
reviewing. `[engine] worker_slots` defaults to `1`, which is the CLI's serial behaviour
and correct for one person at a terminal. It is also what `ns-app serve` runs today, and
it means the *current* serve mode handles one turn at a time across every connected
client: the second user to speak waits for the first user's model calls to finish. That
is a live limitation, not a future one, and it is worth a measurement before anyone
demonstrates the product.

Per tenant it gets worse rather than better, because a company's fifty users would queue
behind each other inside their own dispatcher. So serve mode needs its own default — a
small number of tenant slots under a larger shard-wide cap — while the CLI keeps `1`.
What a slot overlaps is *waiting*, not requests: the throttle and the daily allowance
are per credential and unaffected. That is exactly why more slots are close to free here
and why the current default costs latency for nothing.

**T3.7 A dispatcher's `Err` ends its tenant, never the shard.** Added 2026-09-13 after
reviewing the Phase 2 code as built. Phase 2 classifies `Store` as fatal for the process
under both policies, on the reasoning that every other tenant writes to a store too.
Decision D4 makes that reasoning false: each tenant has its own file, so a corrupt page,
a lock held by something else, or a per-file permission problem belongs to one company
and not to the nineteen beside it. A genuinely shared condition, a full disk, is the
exception rather than the rule.

So the shard-level judgement moves up to where it belongs. `Dispatcher::run` returning
`Err` means *this tenant is finished*; the registry decides what that costs, and only
the registry can see the pattern that justifies ending the process — the same error
arriving from every tenant at once. Phase 2's classification is unchanged and still
correct at its own level; what changes is who reads its verdict.

**T3.8 Quarantine a failing tenant with backoff.** A tenant whose engine fails to build,
or whose dispatcher ends in error, must not be rebuilt on its next message forever. A
permanently broken store would otherwise spin: message, build, fail, message, build,
fail. Hold a failed tenant with exponential backoff and report it, so a broken company
is visibly broken rather than quietly burning the shard's CPU.

**T3.9 `TRACE_TENANT` becomes per tenant.** T1.5 routes the wire trace through a
process-level `OnceLock<String>` set once at startup, which is correct while serve mode
has one tenant and wrong the moment it has two: the second tenant's prompts would land
in the first tenant's file. The sink registry underneath it is already keyed by resolved
path, so it is ready; only the single-valued tenant id has to go.

**CLI identical:** one tenant, one slot, never evicted.

**Exit.** Green; one process serves two tenants with different personas over one port;
no tenant can end the shard on its own.

---

## Phase 4 — identity at the edge

**Why.** H3. The hello is `{token, session}`: a valid token plus any session string
claims that session, and a later claim *displaces the earlier holder*. With one operator
on loopback that is fine. With end users it is impersonation plus a disconnect.

Shape from the research: LiveKit's token (one short-lived, tenant-signed credential with
an opaque subject), Temporal's placement (a credential per namespace, here per tenant).

**T4.1 The resolver seam.** `trait IdentityResolver` returning
`Identity { tenant, session, trust }` from a raw arrival, one implementor per channel.
Everything downstream reads only `Identity`. This is the same kind of injected seam
`scope_for` already is, and the same role Temporal gives a claim mapper.

**T4.2 The web widget resolver.** The hello carries a credential and nothing else:

```rust
struct Hello { token: String }              // was { token, session }

let claims = verify(&h.token, &shared.keys)?;   // signature, exp, iat floor
let session = SessionId(format!("{}/web/{}", claims.iss, claims.sub));
```

The subject is opaque, never an email address or a phone number. It becomes the fact
scope key and is written into a durable log the evolution pass later mines.

**T4.3 Keys and lifetime.** Two active signing keys per tenant so rotation is not an
outage. `exp` enforced. `iat` compared against a per-tenant floor, which is how bulk
revocation works without a revocation list. Keys come from the tenant overlay (T1.2),
read from the environment, never from the repo.

**T4.4 Hello hardening.** A timeout and a length bound on the hello read: a silent
connection currently holds one of a machine-wide pool of connection slots indefinitely.
Refuse a non-loopback bind *before* binding rather than after. Both are named residuals
of the previous plan.

**T4.5 The shared-token mode survives, explicitly.** `[serve] auth = "shared" | "jwt"`.
`shared` keeps today's behaviour for loopback development and the existing tests;
`jwt` is required for any non-loopback bind, and the combination is refused at startup.
The constant-time token comparison already there stays.

**T4.6 Anonymous visitors.** No one has vouched for anything, so the server mints an
opaque unguessable visitor id and that value *is* the credential. `trust = Anonymous`.
Recorded here, enforced in Phase 8 where trust first has a consumer.

**T4.7 Tests.** `a_token_for_tenant_a_cannot_reach_tenant_b`;
`a_client_cannot_choose_its_session_id`; `an_expired_token_is_refused`;
`a_token_signed_by_the_previous_key_is_accepted_during_rotation`;
`a_token_issued_before_the_tenants_floor_is_refused`;
`a_silent_connection_is_dropped_at_the_hello_deadline`;
`a_non_loopback_bind_under_shared_auth_is_refused_at_startup`.

**Exit.** Green; no unauthenticated path reaches an `Engine`; the derived session id is
the only session id the system knows.

---

## Phase 5 — one conversation, several windows

**Why.** H7, and every product in the research syncs a conversation across a user's
tabs and devices. After Phase 4 a takeover is no longer an impersonation — only a user
can collide with themselves — but two tabs still displace each other in a loop.

**T5.1 Fan-out.** The outbound map becomes session to a *set* of connections; a reply
goes to all of them; a departing connection removes only its own entry. The connection
counter needed for that already exists, and the current code already removes an entry
only when it still belongs to the departing connection, so this generalises rather than
being rewritten. Outbound stays a bounded `try_send` per connection: one peer that stops
reading must not stall the session task.

**T5.2 Tests.** `two_connections_on_one_session_both_receive_the_reply`;
`closing_one_of_two_connections_leaves_the_other_serving`;
`a_stalled_connection_does_not_delay_the_other_holder_of_its_session`.

---

## Phase 6 — WhatsApp, or any webhook channel

**Why.** It is the second channel, and it breaks four assumptions the TCP channel bakes
in. Doing it before the architecture sets means the resolver seam and the outbound path
are shaped by two channels rather than one.

**The trust chain is different.** The customer id in the payload is an *identifier*,
not a credential. What is verified is Meta's signature: an HMAC-SHA256 over the **raw**
request body with the app secret, compared in constant time, delivered in
`X-Hub-Signature-256`. The tenant likewise comes from the business phone number id
*inside the verified payload*, never from anything the sender chose.

**T6.1 Ingress.** An HTTP endpoint per shard. Verify the signature over the raw bytes
before parsing — a body that has been through a JSON round trip will not hash. Persist
the message, acknowledge with 200 quickly, then enqueue: a turn takes seconds and a
webhook handler must not.

**T6.2 H4: dedupe at ingress.** Meta retries any non-200 for up to seven days and may
batch many entries in one POST. Deduplicate on the platform message id *before* the
mailbox. The engine's idempotent append protects the log but would not stop a turn
running twice, replying twice and billing twice. A small persistent seen-set per tenant,
not an in-memory one: retries outlive a restart.

**T6.3 Order within a batch.** Per-session ordering is guaranteed by the mailbox, but a
batch can arrive out of order. Sort by the platform timestamp before enqueuing.

**T6.4 The resolver.** Verified signature, then tenant from the business phone number
id, session as tenant plus channel plus a per-tenant salted hash of the customer id,
trust as platform-verified. The mapping from hash back to number lives in a side table
only the resolver reads. A phone number must not become a fact scope key.

**T6.5 Outbound has no connection.** Replies are an API call carrying the tenant's own
access token, with their own retries and their own failure mode. The TCP channel logs a
dropped reply when no connection holds a session, treating it as an anomaly; here that
is the normal and only state. Reuse the existing `ToolTransport` trait so the send path
is testable against `MockToolTransport` rather than the network.

**T6.6 The send window.** Business-initiated messages outside a limited window after the
customer's last message require pre-approved templates. Confirm the current rule against
Meta's documentation at implementation time; it is the constraint most likely to have
changed, and it shapes anything proactive.

**T6.7 Tests.** `a_payload_with_a_bad_signature_is_refused_before_parsing`;
`a_repeated_message_id_runs_one_turn` (H4);
`a_batch_is_enqueued_in_timestamp_order`;
`a_customer_id_never_appears_in_the_session_id_or_the_fact_scope`;
`an_outbound_send_failure_is_retried_and_then_logged`;
`two_tenants_on_one_endpoint_are_separated_by_phone_number_id`.

**Exit.** Green; one shard serves a web tenant and a WhatsApp tenant at once, and the
engine cannot tell them apart.

---

## Phase 7 — scale-out and operations, gated

Trigger: the T0.1 number against the real tenant count, or the first tenant that
outgrows a shard.

- Sticky tenant routing at a stateless gateway; shards as a StatefulSet with a volume
  each; a tenant-to-shard table rather than a hash, so a noisy tenant can be moved.
- Continuous replication of each tenant's SQLite to object storage. A pod restart is
  seconds of downtime for its tenants; volume loss must not be data loss.
- Per-tenant provisioning and deprovisioning as one command each: keys, overlay, store,
  and a deletion that is one file.
- Do not do this early. One machine running a few shards under service units is the same
  architecture with less to operate, and the measured ceiling of a single process is far
  above what any provider will sell in requests.

---

## Phase 8 — gated, named so they are not forgotten

- **Per-session tool grants** from the token's `grants` claim, and the first consumer of
  `trust`. Tools are registered once at assembly, so enforcing a per-session subset needs
  a filter the turn loop does not have. Its own work, not authentication's.
- **Anonymous to identified.** Do not rewrite history: seed the identified session with
  the anonymous session's digest, which is already the artifact for carrying a
  conversation elsewhere, and leave the old log where it is.
- **Session rollover (Continue-As-New).** Per-tenant store files make its trigger per
  tenant rather than global.
- **Per-tenant learned rules and guidance.** The evolution pass currently mines every
  session under one digest scope. Until it is tenant-scoped, run it per tenant or not at
  all in serve mode; a rule learned from A's traffic must not reach B.

---

## Sequencing

Phases 1 and 2 are independent of each other and both block 3. Phase 4 blocks any
non-loopback deployment and should not wait for 3 if a pilot is coming: it can land on
the single-tenant serve path first. Phase 5 is small and independent. Phase 6 wants 4's
resolver seam in place.

A defensible first cut for one pilot customer: 1, 2, 4. That is a single-tenant serve
deployment that is safe to expose, with the factory already extracted so the second
tenant is configuration rather than a rewrite.

## Results

| item | value | phase |
|---|---|---|
| resident memory per idle engine | | 0 |
| build time per engine | | 0 |
| tenants per shard (derived) | | 0 |
| turn failure isolation, CLI unchanged | | 2 |

## Open questions

1. Does a tenant's evolution pass run at all in serve mode before Phase 8 scopes it?
   Proposed answer: no, off in serve.
2. Is `tenant_idle_after` the same knob as the session `idle_after`, or its own? It
   should be its own and much longer; building an engine is not free (T0.1).
3. Where do tenant overlays live once there are a hundred — files, or a table? Files
   until they hurt, because a file is reviewable and diffable.

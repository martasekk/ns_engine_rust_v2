//! Which companies have an engine up, and who is allowed to build one.
//!
//! Plan `.claude/plans/continue-polymorphic-pearl.md` Part B, steps B4, B5
//! and B8. One socket feeds every company in the process (A1), and the
//! listener names a company the moment it speaks with nobody draining its
//! queue. Turning that name into a running engine is this module's whole
//! job, and the three things it must get right are:
//!
//! * **One engine per company, however many messages race.** A per-company
//!   build lock, and a re-check of the live set once it is held. Two first
//!   messages arriving together must not open one store file twice.
//! * **Eviction that loses nothing.** The tenant channel is dropped
//!   *before* the company is unregistered, because dropping it parks the
//!   receiver back where a later resolve can take it and re-wakes the
//!   stream if anything arrived meanwhile. Reversed, a resolve racing the
//!   teardown finds a queue with no receiver in it and hands back nothing.
//! * **A company's failure is its own.** One store error is one company's
//!   corrupt file; the same error from several companies inside a window is
//!   the disk, and that ends the shard (B5).
//!
//! The registry is generic over what a build produces so tests can inject a
//! double: the real factory is `pub(crate)` and building a real engine
//! dials providers and opens sqlite files, neither of which belongs in a
//! test about who builds what and when. Production passes a closure over
//! `factory::build_engine`, whose output already implements [`RunTenant`]
//! (`main::tenant_builder`, plan B9).
//!
//! This file is the map. `TenantRegistry` itself lives here - the build
//! lock, the eviction ordering and the wake loop - and each of the concerns
//! it is assembled from lives in a sibling named after it:
//!
//! | module      | what it owns                                        |
//! |-------------|-----------------------------------------------------|
//! | [`wiring`]  | the three edges to the world, each behind a trait    |
//! | [`stats`]   | what a company cost, counted on its own channel      |
//! | [`limits`]  | the knobs, and the backoff arithmetic                |
//! | [`failure`] | what a failure costs, and how long it is waited out  |

mod failure;
mod limits;
mod stats;
mod wiring;

// The names this module was a single file under, kept exactly as they were:
// `registry::TenantRegistry`, `registry::RegistryLimits` and
// `registry::BuildTenant` are what `main` builds a shard out of, and a split
// is not a reason to rewrite its imports.
pub(crate) use limits::RegistryLimits;
pub(crate) use wiring::{BuildTenant, RunTenant, TenantSource};

use failure::{Quarantine, RegistryError, ShardFatal};
use limits::backoff_for;
use nscore::Channel;
use nsengine::dispatch::ShardSlots;
use stats::{Activity, CountedChannel, TenantStats};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// A `Mutex` that a panic cannot take out of service.
///
/// Same discipline as the outbound map in `channel-tcp`: with many
/// companies in one process, one panicked turn must not turn every later
/// lock of a shared map into a second panic.
fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// One company with an engine up.
struct Live {
    /// `Option` because eviction takes it out and drops it *before* the
    /// entry is removed. See [`TenantRegistry::unregister`].
    channel: Option<Arc<dyn Channel>>,
    task: Option<JoinHandle<()>>,
    activity: Arc<Activity>,
}

/// One process, many companies.
pub(crate) struct TenantRegistry<R: RunTenant> {
    build: BuildTenant<R>,
    source: Arc<dyn TenantSource>,
    shard: ShardSlots,
    limits: RegistryLimits,
    live: Mutex<HashMap<String, Live>>,
    /// One lock per company, held across its whole build. Two first
    /// messages for the same cold company therefore build one engine: the
    /// second waits, then finds the first one live.
    building: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    quarantine: Mutex<HashMap<String, Quarantine>>,
    stats: Mutex<HashMap<String, Arc<TenantStats>>>,
    /// Store errors seen recently, at most one entry per company: the
    /// window B5 decides "one corrupt file" against "the disk" in.
    store_errors: Mutex<Vec<(String, Instant)>>,
    fatal: Mutex<Option<ShardFatal>>,
    /// `notify_one` rather than `notify_waiters`, so a failure landing
    /// before the serve loop waits is not lost.
    fatal_notify: Notify,
}

impl<R: RunTenant> TenantRegistry<R> {
    pub(crate) fn new(
        build: BuildTenant<R>,
        source: Arc<dyn TenantSource>,
        shard: ShardSlots,
        limits: RegistryLimits,
    ) -> Self {
        Self {
            build,
            source,
            shard,
            limits,
            live: Mutex::new(HashMap::new()),
            building: Mutex::new(HashMap::new()),
            quarantine: Mutex::new(HashMap::new()),
            stats: Mutex::new(HashMap::new()),
            store_errors: Mutex::new(Vec::new()),
            fatal: Mutex::new(None),
            fatal_notify: Notify::new(),
        }
    }

    /// Make sure this company has an engine draining its queue.
    ///
    /// Idempotent and safe to call from anywhere: a company that is already
    /// live, or whose queue somebody else has taken, is `Ok(())` with
    /// nothing built.
    async fn resolve(self: &Arc<Self>, tenant: &str) -> Result<(), RegistryError> {
        if let Some(retry_in) = self.quarantined_for(tenant) {
            return Err(RegistryError::Quarantined {
                tenant: tenant.to_string(),
                retry_in,
            });
        }
        let gate = self.build_gate(tenant);
        let _held = gate.lock().await;
        // Both re-checked under the lock: whoever held it before us may have
        // built this company, or failed building it.
        if locked(&self.live).contains_key(tenant) {
            return Ok(());
        }
        if let Some(retry_in) = self.quarantined_for(tenant) {
            return Err(RegistryError::Quarantined {
                tenant: tenant.to_string(),
                retry_in,
            });
        }
        // No queue, or somebody already drains it. Nothing to do and not a
        // failure: a company has one engine.
        let Some(inbound) = self.source.tenant_channel(tenant) else {
            return Ok(());
        };
        let stats = self.stats_for(tenant);
        let activity = Arc::new(Activity::new());
        let channel: Arc<dyn Channel> = Arc::new(CountedChannel {
            inner: inbound,
            stats: stats.clone(),
            activity: activity.clone(),
        });
        stats.builds.fetch_add(1, Ordering::SeqCst);
        let built = match (self.build)(tenant.to_string(), channel.clone()).await {
            Ok(built) => built,
            Err(e) => {
                stats.failures.fetch_add(1, Ordering::SeqCst);
                let retry_in = self.quarantine(tenant);
                let detail = e.to_string();
                eprintln!(
                    "tenant {tenant}: {detail}; not retried for {}s",
                    retry_in.as_secs()
                );
                return Err(RegistryError::Build {
                    tenant: tenant.to_string(),
                    detail,
                });
            }
        };
        // Registered before the task is spawned, so a run that ends
        // immediately finds the entry it has to clear.
        locked(&self.live).insert(
            tenant.to_string(),
            Live {
                channel: Some(channel),
                task: None,
                activity,
            },
        );
        let me = Arc::downgrade(self);
        let id = tenant.to_string();
        let shard = self.shard.clone();
        let task = tokio::spawn(async move {
            let outcome = built.run(shard).await;
            if let Some(registry) = me.upgrade() {
                registry.finish(&id, outcome);
            }
        });
        match locked(&self.live).get_mut(tenant) {
            Some(live) => live.task = Some(task),
            // Already finished and unregistered itself. Nothing to hold.
            None => task.abort(),
        }
        Ok(())
    }

    /// Drain the wake stream until the listener goes or the shard does.
    ///
    /// `Err` is the one thing a company cannot decide for itself: enough
    /// store failures across enough companies that the disk, not a file, is
    /// the explanation.
    pub(crate) async fn serve(self: Arc<Self>) -> Result<(), ShardFatal> {
        let mut sweep = tokio::time::interval(self.limits.sweep_every);
        loop {
            // Taken out of the lock before any await: a guard held across
            // one makes this future `!Send`, and it is spawned.
            let fatal = locked(&self.fatal).take();
            if let Some(fatal) = fatal {
                self.shutdown().await;
                return Err(fatal);
            }
            let woke = tokio::select! {
                biased;
                () = self.fatal_notify.notified() => continue,
                next = self.source.next_active_tenant() => next,
                _ = sweep.tick() => {
                    self.sweep_idle().await;
                    continue;
                }
            };
            // The listener is gone: the shard is shutting down.
            let Some(tenant) = woke else { break };
            if let Err(e) = self.resolve(&tenant).await {
                eprintln!("{e}");
            }
        }
        self.shutdown().await;
        let fatal = locked(&self.fatal).take();
        match fatal {
            Some(fatal) => Err(fatal),
            None => Ok(()),
        }
    }

    /// Tear this company's engine down: stop the run, drop the channel,
    /// then unregister.
    async fn evict(&self, tenant: &str) {
        let task = locked(&self.live)
            .get_mut(tenant)
            .and_then(|live| live.task.take());
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
        self.unregister(tenant);
    }

    /// Every company that has been quiet longer than the eviction knob.
    async fn sweep_idle(&self) {
        let stale: Vec<String> = locked(&self.live)
            .iter()
            .filter(|(_, live)| live.activity.idle_for() >= self.limits.idle_evict_after)
            .map(|(id, _)| id.clone())
            .collect();
        for tenant in stale {
            eprintln!("tenant {tenant}: idle, engine released");
            self.evict(&tenant).await;
        }
    }

    /// This company's counters, created on first mention so a company that
    /// has only ever failed still has a row.
    fn stats_for(&self, tenant: &str) -> Arc<TenantStats> {
        locked(&self.stats)
            .entry(tenant.to_string())
            .or_default()
            .clone()
    }

    /// Every company's counters, one line each, in the words an operator
    /// reads rather than a debugger (plan B8).
    pub(crate) fn report(&self) -> String {
        let mut ids: Vec<String> = locked(&self.stats).keys().cloned().collect();
        ids.sort();
        let mut out = String::new();
        for id in ids {
            let s = self.stats_for(&id);
            let state = if self.is_live(&id) {
                "live"
            } else if self.quarantined_for(&id).is_some() {
                "quarantined"
            } else {
                "idle"
            };
            out.push_str(&format!(
                "tenant {id}: turns={} replies={} in_flight={} failures={} builds={} \
                 requests={} state={state}\n",
                s.turns(),
                s.replies(),
                s.in_flight(),
                s.failures(),
                s.builds(),
                s.requests(),
            ));
        }
        out
    }

    /// Whether this company is live right now.
    fn is_live(&self, tenant: &str) -> bool {
        locked(&self.live).contains_key(tenant)
    }

    /// The ordering rule, in one place.
    ///
    /// The channel is dropped first and the entry removed second. Dropping
    /// the channel parks the receiver back where [`TenantSource::
    /// tenant_channel`] can take it and re-wakes the stream if anything
    /// arrived while it was being dropped; removing the entry is what lets
    /// a resolve act on that wake. Reversed, a resolve landing between the
    /// two finds the company unregistered and its queue undrainable, and
    /// the message that woke it waits for the next one.
    fn unregister(&self, tenant: &str) {
        let channel = locked(&self.live)
            .get_mut(tenant)
            .and_then(|live| live.channel.take());
        drop(channel);
        locked(&self.live).remove(tenant);
    }

    /// This company's build lock, created on demand.
    fn build_gate(&self, tenant: &str) -> Arc<tokio::sync::Mutex<()>> {
        locked(&self.building)
            .entry(tenant.to_string())
            .or_default()
            .clone()
    }

    /// Let every company go, waiting for each run to stop.
    async fn shutdown(&self) {
        let ids: Vec<String> = locked(&self.live).keys().cloned().collect();
        for tenant in ids {
            self.evict(&tenant).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::failure::ShardVerdict;
    use super::wiring::BoxFuture;
    use super::*;
    use crate::factory::StartupError;
    use async_trait::async_trait;
    use nscore::{ChannelError, Incoming, SessionId};
    use nsengine::turn::EngineError;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicBool;
    use std::sync::Weak;
    use std::time::Duration;
    use tokio::sync::mpsc;

    /// A listener the test drives by hand.
    ///
    /// Deliberately *permissive* where the real `TcpChannel` is strict:
    /// `tenant_channel` hands out a receiver whenever the queue exists,
    /// rather than only when it is parked. The crate's single-take rule is
    /// proven in the crate's own tests, and a double that repeated it would
    /// hide whether the registry has a guard of its own - which is the
    /// whole subject of `two_messages_for_a_cold_tenant_build_one_engine`.
    ///
    /// Everything else mirrors the crate: dropping a tenant channel parks
    /// the receiver and re-wakes the stream if anything arrived meanwhile,
    /// and a company's first message wakes the stream once.
    struct FakeListener {
        queues: Mutex<HashMap<String, Queue>>,
        wake_tx: Mutex<Option<mpsc::UnboundedSender<String>>>,
        wake_rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<String>>,
        closed: AtomicBool,
        /// A handle back to itself, so a tenant channel can reach the
        /// queues without the listener being passed in twice.
        me: Mutex<Weak<FakeListener>>,
    }

    #[derive(Default)]
    struct Queue {
        messages: VecDeque<Incoming>,
        /// No engine is draining it.
        parked: bool,
        /// Already named on the wake stream and not yet taken.
        woken: bool,
        notify: Arc<Notify>,
        replies: Vec<(String, String)>,
    }

    impl FakeListener {
        fn new() -> Arc<Self> {
            let (tx, rx) = mpsc::unbounded_channel();
            let listener = Arc::new(Self {
                queues: Mutex::new(HashMap::new()),
                wake_tx: Mutex::new(Some(tx)),
                wake_rx: tokio::sync::Mutex::new(rx),
                closed: AtomicBool::new(false),
                me: Mutex::new(Weak::new()),
            });
            *locked(&listener.me) = Arc::downgrade(&listener);
            listener
        }

        /// One message from one of this company's connections.
        fn deliver(&self, tenant: &str, text: &str) {
            let mut queues = locked(&self.queues);
            let q = queues.entry(tenant.to_string()).or_insert_with(|| Queue {
                parked: true,
                ..Queue::default()
            });
            q.messages.push_back(Incoming {
                session: SessionId(format!("{tenant}/s1")),
                text: text.to_string(),
            });
            let notify = q.notify.clone();
            let wake = if q.parked && !q.woken {
                q.woken = true;
                true
            } else {
                false
            };
            drop(queues);
            notify.notify_one();
            if wake {
                self.wake(tenant);
            }
        }

        fn wake(&self, tenant: &str) {
            if let Some(tx) = locked(&self.wake_tx).as_ref() {
                let _ = tx.send(tenant.to_string());
            }
        }

        /// The listener going away: queues drain, then report closed, and
        /// the wake stream ends.
        fn close(&self) {
            self.closed.store(true, Ordering::SeqCst);
            let notifies: Vec<Arc<Notify>> = locked(&self.queues)
                .values()
                .map(|q| q.notify.clone())
                .collect();
            for n in notifies {
                n.notify_waiters();
                n.notify_one();
            }
            locked(&self.wake_tx).take();
        }

        fn replies(&self, tenant: &str) -> Vec<(String, String)> {
            locked(&self.queues)
                .get(tenant)
                .map(|q| q.replies.clone())
                .unwrap_or_default()
        }
    }

    impl TenantSource for FakeListener {
        fn tenant_channel(&self, tenant: &str) -> Option<Arc<dyn Channel>> {
            let mut queues = locked(&self.queues);
            let q = queues.get_mut(tenant)?;
            q.parked = false;
            q.woken = false;
            let notify = q.notify.clone();
            drop(queues);
            Some(Arc::new(FakeTenantChannel {
                tenant: tenant.to_string(),
                listener: locked(&self.me).clone(),
                notify,
            }))
        }

        fn next_active_tenant(&self) -> BoxFuture<'_, Option<String>> {
            Box::pin(async move { self.wake_rx.lock().await.recv().await })
        }
    }

    struct FakeTenantChannel {
        tenant: String,
        listener: Weak<FakeListener>,
        notify: Arc<Notify>,
    }

    #[async_trait]
    impl Channel for FakeTenantChannel {
        async fn recv(&self) -> Result<Incoming, ChannelError> {
            loop {
                {
                    let Some(listener) = self.listener.upgrade() else {
                        return Err(ChannelError::Closed);
                    };
                    let mut queues = locked(&listener.queues);
                    if let Some(q) = queues.get_mut(&self.tenant) {
                        if let Some(m) = q.messages.pop_front() {
                            return Ok(m);
                        }
                    }
                    if listener.closed.load(Ordering::SeqCst) {
                        return Err(ChannelError::Closed);
                    }
                }
                self.notify.notified().await;
            }
        }

        async fn send(&self, session: &SessionId, text: &str) -> Result<(), ChannelError> {
            let Some(listener) = self.listener.upgrade() else {
                return Err(ChannelError::Closed);
            };
            let mut queues = locked(&listener.queues);
            if let Some(q) = queues.get_mut(&self.tenant) {
                q.replies.push((session.0.clone(), text.to_string()));
            }
            Ok(())
        }
    }

    impl Drop for FakeTenantChannel {
        /// The crate's rule: the receiver goes back, and anything that
        /// arrived while it was going wakes whoever is watching.
        fn drop(&mut self) {
            let Some(listener) = self.listener.upgrade() else {
                return;
            };
            let mut queues = locked(&listener.queues);
            let Some(q) = queues.get_mut(&self.tenant) else {
                return;
            };
            q.parked = true;
            let wake = !q.messages.is_empty() && !q.woken;
            if wake {
                q.woken = true;
            }
            drop(queues);
            if wake {
                listener.wake(&self.tenant);
            }
        }
    }

    /// What a built double does: take `take` messages, then either fail
    /// with `then` or park forever.
    ///
    /// The failure is a constructor rather than a value because
    /// `EngineError` is not `Clone` and a company's last script repeats for
    /// every later build of it.
    #[derive(Clone)]
    struct Script {
        take: usize,
        then: Option<Arc<dyn Fn() -> EngineError + Send + Sync>>,
    }

    impl Script {
        fn serving() -> Self {
            Self {
                take: usize::MAX,
                then: None,
            }
        }
        fn takes(n: usize) -> Self {
            Self {
                take: n,
                then: None,
            }
        }
        fn takes_then_fails(n: usize, e: impl Fn() -> EngineError + Send + Sync + 'static) -> Self {
            Self {
                take: n,
                then: Some(Arc::new(e)),
            }
        }
        fn fails(e: impl Fn() -> EngineError + Send + Sync + 'static) -> Self {
            Self::takes_then_fails(0, e)
        }
    }

    struct FakeRun {
        tenant: String,
        channel: Arc<dyn Channel>,
        script: Script,
        seen: mpsc::UnboundedSender<(String, String)>,
    }

    impl RunTenant for FakeRun {
        fn run(self, _shard: ShardSlots) -> BoxFuture<'static, Result<(), EngineError>> {
            Box::pin(async move {
                for _ in 0..self.script.take {
                    match self.channel.recv().await {
                        Ok(incoming) => {
                            let _ = self.channel.send(&incoming.session, "ok").await;
                            let _ = self.seen.send((self.tenant.clone(), incoming.text));
                        }
                        // The listener went: this run is over, cleanly.
                        Err(_) => return Ok(()),
                    }
                }
                match self.script.then {
                    Some(fail) => Err(fail()),
                    // Nothing left to do and nothing wrong: stay up, as a
                    // real dispatcher between messages does.
                    None => std::future::pending().await,
                }
            })
        }
    }

    /// The whole fixture: a listener, a build closure over scripts, and the
    /// stream of what the engines saw.
    struct Fixture {
        listener: Arc<FakeListener>,
        registry: Arc<TenantRegistry<FakeRun>>,
        seen: mpsc::UnboundedReceiver<(String, String)>,
        builds: Arc<Mutex<Vec<String>>>,
    }

    /// `scripts` is consulted per build: the first build of a company takes
    /// the first entry, the second the second, and the last entry repeats.
    fn fixture(scripts: Vec<(&str, Vec<Script>)>, limits: RegistryLimits) -> Fixture {
        fixture_with(scripts, limits, None)
    }

    /// `build_error` makes every build fail, which is the permanently
    /// broken company.
    fn fixture_with(
        scripts: Vec<(&str, Vec<Script>)>,
        limits: RegistryLimits,
        build_error: Option<&'static str>,
    ) -> Fixture {
        let listener = FakeListener::new();
        let (seen_tx, seen_rx) = mpsc::unbounded_channel();
        let builds = Arc::new(Mutex::new(Vec::<String>::new()));
        let mut table: HashMap<String, VecDeque<Script>> = HashMap::new();
        for (id, list) in scripts {
            table.insert(id.to_string(), list.into_iter().collect());
        }
        let table = Arc::new(Mutex::new(table));
        let builds_for_closure = builds.clone();
        let build: BuildTenant<FakeRun> = Arc::new(move |tenant, channel| {
            let builds = builds_for_closure.clone();
            let table = table.clone();
            let seen = seen_tx.clone();
            Box::pin(async move {
                // An await point inside the build, so two racing resolves
                // really do interleave rather than run to completion one
                // after the other.
                tokio::task::yield_now().await;
                locked(&builds).push(tenant.clone());
                if let Some(detail) = build_error {
                    return Err(StartupError::Config(detail.to_string()));
                }
                let script = {
                    let mut table = locked(&table);
                    let list = table.entry(tenant.clone()).or_default();
                    if list.len() > 1 {
                        list.pop_front().expect("checked")
                    } else {
                        // The last entry repeats: a company with one script
                        // behaves the same way every time it is built.
                        list.front().cloned().unwrap_or_else(Script::serving)
                    }
                };
                Ok(FakeRun {
                    tenant,
                    channel,
                    script,
                    seen,
                })
            })
        });
        let registry = Arc::new(TenantRegistry::new(
            build,
            listener.clone(),
            ShardSlots::new(4),
            limits,
        ));
        Fixture {
            listener,
            registry,
            seen: seen_rx,
            builds,
        }
    }

    /// The next message an engine took, with a ceiling so a regression
    /// fails the test instead of hanging it. The ceiling is a failure
    /// guard, not the synchronisation: a passing run never waits on it.
    async fn next_seen(rx: &mut mpsc::UnboundedReceiver<(String, String)>) -> (String, String) {
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Some(seen)) => seen,
            Ok(None) => panic!("the seen stream ended with no message"),
            Err(_) => panic!("no engine took a message within 5s"),
        }
    }

    fn store_error(detail: &str) -> EngineError {
        EngineError::Store(nscore::StoreError::Io(detail.to_string()))
    }

    /// Two messages for a company with no engine build one engine, not two.
    ///
    /// Two engines would be two opens of one store file, which is the
    /// hazard the per-company build lock exists for: the second resolve
    /// waits on the lock and then finds the first one's engine live.
    #[tokio::test]
    async fn two_messages_for_a_cold_tenant_build_one_engine() {
        let f = fixture(
            vec![("acme", vec![Script::serving()])],
            RegistryLimits::default(),
        );
        f.listener.deliver("acme", "first");
        f.listener.deliver("acme", "second");
        let (a, b) = tokio::join!(f.registry.resolve("acme"), f.registry.resolve("acme"));
        assert!(a.is_ok(), "{a:?}");
        assert!(b.is_ok(), "{b:?}");
        assert_eq!(
            locked(&f.builds).as_slice(),
            ["acme".to_string()],
            "two racing messages must build one engine"
        );
        assert!(f.registry.is_live("acme"));
    }

    /// A company whose engine was released gets another one when it speaks
    /// again. Eviction is a release, not a ban.
    #[tokio::test]
    async fn an_evicted_tenant_is_rebuilt_on_its_next_message() {
        let mut f = fixture(
            vec![("acme", vec![Script::serving()])],
            RegistryLimits::default(),
        );
        f.listener.deliver("acme", "one");
        f.registry
            .resolve("acme")
            .await
            .expect("a cold company builds");
        assert_eq!(next_seen(&mut f.seen).await, ("acme".into(), "one".into()));

        f.registry.evict("acme").await;
        assert!(!f.registry.is_live("acme"), "eviction unregisters");

        f.listener.deliver("acme", "two");
        f.registry
            .resolve("acme")
            .await
            .expect("a warm queue rebuilds");
        assert_eq!(next_seen(&mut f.seen).await, ("acme".into(), "two".into()));
        assert_eq!(locked(&f.builds).len(), 2, "one engine per period of life");
    }

    /// A message that arrives while a company's engine is being torn down
    /// is delivered, not lost.
    ///
    /// The channel re-wakes the stream when it is dropped with messages
    /// still queued. This is the registry acting on that wake: the serve
    /// loop is what has to be still draining, and the resolve it makes has
    /// to find the queue takeable - which is why the channel is dropped
    /// before the company is unregistered.
    #[tokio::test]
    async fn a_message_landing_during_tenant_teardown_is_not_lost() {
        // The first engine takes exactly one message and then parks, so the
        // second message provably waits in the queue rather than racing the
        // engine for it.
        let mut f = fixture(
            vec![("acme", vec![Script::takes(1), Script::serving()])],
            RegistryLimits::default(),
        );
        let registry = f.registry.clone();
        let serving = tokio::spawn(async move { registry.serve().await });

        f.listener.deliver("acme", "first");
        assert_eq!(
            next_seen(&mut f.seen).await,
            ("acme".into(), "first".into())
        );

        // Queued while the engine is up and not reading: no wake yet.
        f.listener.deliver("acme", "second");
        f.registry.evict("acme").await;

        // Nothing else nudges the registry. Only the re-wake from the
        // dropped channel can get this message delivered.
        assert_eq!(
            next_seen(&mut f.seen).await,
            ("acme".into(), "second".into())
        );

        f.listener.close();
        let out = serving.await.expect("the serve loop joins");
        assert!(out.is_ok(), "{out:?}");
    }

    /// One company's store error is one company's problem: it has its own
    /// database file, and the shard keeps serving everybody else.
    #[tokio::test]
    async fn a_tenants_store_error_does_not_stop_another_tenant() {
        let mut f = fixture(
            vec![
                (
                    "acme",
                    vec![Script::fails(|| store_error("acme.sqlite is corrupt"))],
                ),
                ("beta", vec![Script::serving()]),
            ],
            RegistryLimits::default(),
        );
        f.listener.deliver("beta", "hello");
        f.registry.resolve("beta").await.expect("beta builds");
        assert_eq!(
            next_seen(&mut f.seen).await,
            ("beta".into(), "hello".into())
        );

        f.listener.deliver("acme", "hello");
        f.registry
            .resolve("acme")
            .await
            .expect("acme builds, then fails");
        // The failing run unregisters itself; wait for that rather than
        // assuming it has happened.
        for _ in 0..1000 {
            if !f.registry.is_live("acme") {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(!f.registry.is_live("acme"), "a failed company is released");
        assert!(
            f.registry.quarantined_for("acme").is_some(),
            "and quarantined rather than rebuilt per message"
        );

        // beta neither noticed nor stopped.
        assert!(f.registry.is_live("beta"), "beta keeps its engine");
        f.listener.deliver("beta", "still here");
        assert_eq!(
            next_seen(&mut f.seen).await,
            ("beta".into(), "still here".into())
        );
        assert_eq!(f.registry.stats_for("beta").failures(), 0);
    }

    /// Store errors from several distinct companies inside the window are
    /// the disk, and the disk ends the shard.
    ///
    /// The counting is per company, not per error: one company failing over
    /// and over is still one company.
    #[tokio::test]
    async fn a_store_error_from_every_tenant_ends_the_shard() {
        let limits = RegistryLimits {
            store_fatal_tenants: 3,
            ..RegistryLimits::default()
        };
        let f = fixture(vec![], limits);
        let r = &f.registry;

        for _ in 0..5 {
            assert!(
                matches!(
                    r.on_tenant_error("acme", &store_error("one file")),
                    ShardVerdict::TenantOnly
                ),
                "one company's file is one company's problem, however often"
            );
        }
        assert!(matches!(
            r.on_tenant_error("beta", &store_error("another file")),
            ShardVerdict::TenantOnly
        ));
        let verdict = r.on_tenant_error("gamma", &store_error("a third file"));
        let fatal = match verdict {
            ShardVerdict::ShardFatal(f) => f,
            ShardVerdict::TenantOnly => panic!("three distinct companies is the disk"),
        };
        assert_eq!(fatal.tenants.len(), 3, "{fatal}");
        assert!(fatal.to_string().contains("acme"), "{fatal}");

        // The other two variants are one company's, whatever the company.
        assert!(matches!(
            r.on_tenant_error("delta", &EngineError::RequestCap { spent: 9, cap: 9 }),
            ShardVerdict::TenantOnly
        ));
        assert!(matches!(
            r.on_tenant_error("delta", &EngineError::Channel("gone".into())),
            ShardVerdict::TenantOnly
        ));
    }

    /// A company that cannot be built is not rebuilt on every message it
    /// sends. It is quarantined, reported once, and retried with a backoff
    /// that doubles.
    #[tokio::test]
    async fn a_permanently_broken_tenant_is_not_rebuilt_on_every_message() {
        let f = fixture_with(
            vec![],
            RegistryLimits::default(),
            Some("no api key for role replier"),
        );
        f.listener.deliver("acme", "hello");
        let first = f.registry.resolve("acme").await;
        assert!(
            matches!(first, Err(RegistryError::Build { .. })),
            "{first:?}"
        );

        for _ in 0..5 {
            f.listener.deliver("acme", "hello again");
            match f.registry.resolve("acme").await {
                Err(RegistryError::Quarantined { tenant, .. }) => assert_eq!(tenant, "acme"),
                other => panic!("a broken company must be refused, not rebuilt: {other:?}"),
            }
        }
        assert_eq!(
            locked(&f.builds).len(),
            1,
            "six messages, one build attempt"
        );
        assert_eq!(f.registry.stats_for("acme").failures(), 1);

        // And the wait doubles rather than staying flat, up to the ceiling.
        let base = Duration::from_secs(5);
        let max = Duration::from_secs(60);
        assert_eq!(backoff_for(1, base, max), Duration::from_secs(5));
        assert_eq!(backoff_for(2, base, max), Duration::from_secs(10));
        assert_eq!(backoff_for(3, base, max), Duration::from_secs(20));
        assert_eq!(backoff_for(9, base, max), max, "capped");
    }

    /// Every counter is filed under the company that earned it, and one
    /// company's traffic never lands on another's row.
    #[tokio::test]
    async fn a_tenants_turns_and_spend_are_counted_under_its_own_id() {
        let mut f = fixture(
            vec![
                (
                    "acme",
                    vec![Script::takes_then_fails(1, || EngineError::RequestCap {
                        spent: 42,
                        cap: 42,
                    })],
                ),
                ("beta", vec![Script::serving()]),
            ],
            RegistryLimits::default(),
        );
        f.listener.deliver("beta", "beta one");
        f.registry.resolve("beta").await.expect("beta builds");
        assert_eq!(
            next_seen(&mut f.seen).await,
            ("beta".into(), "beta one".into())
        );

        f.listener.deliver("acme", "acme one");
        f.registry.resolve("acme").await.expect("acme builds");
        assert_eq!(
            next_seen(&mut f.seen).await,
            ("acme".into(), "acme one".into())
        );
        for _ in 0..1000 {
            if f.registry.stats_for("acme").requests() > 0 {
                break;
            }
            tokio::task::yield_now().await;
        }

        let acme = f.registry.stats_for("acme");
        let beta = f.registry.stats_for("beta");
        assert_eq!(acme.turns(), 1, "acme took one message");
        assert_eq!(beta.turns(), 1, "and so did beta, on its own row");
        assert_eq!(acme.replies(), 1);
        assert_eq!(acme.in_flight(), 0, "answered, so nothing in flight");
        assert_eq!(acme.requests(), 42, "the engine's own spend, under acme");
        assert_eq!(beta.requests(), 0, "and none of it under beta");
        assert_eq!(acme.failures(), 1, "the capped run ended in error");
        assert_eq!(beta.failures(), 0);
        assert_eq!(f.listener.replies("beta").len(), 1);

        // Readable without a debugger.
        let report = f.registry.report();
        assert!(report.contains("tenant acme: turns=1"), "{report}");
        assert!(report.contains("requests=42"), "{report}");
        assert!(report.contains("tenant beta: turns=1"), "{report}");
    }
}

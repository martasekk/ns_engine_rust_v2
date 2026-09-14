//! What a company's failure costs, and how long it is waited out.
//!
//! Two decisions, and neither of them touches a store, an engine or a
//! channel: whether this failure is one company's or the shard's, and when
//! the company may be built again.

use super::{backoff_for, locked, RunTenant, TenantRegistry};
use nsengine::turn::EngineError;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// A company that failed and is not being retried yet.
pub(super) struct Quarantine {
    attempts: u32,
    until: Instant,
}

/// What one company's failure costs.
#[derive(Debug)]
pub(super) enum ShardVerdict {
    /// This company and nobody else.
    TenantOnly,
    /// Every company here. The shard stops.
    ShardFatal(ShardFatal),
}

/// Why the whole shard stopped, in the words it is reported in.
#[derive(Debug)]
pub(crate) struct ShardFatal {
    pub reason: String,
    /// The companies whose failures added up to it.
    pub tenants: Vec<String>,
}

impl std::fmt::Display for ShardFatal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.reason, self.tenants.join(", "))
    }
}

impl std::error::Error for ShardFatal {}

/// Why a resolve did not leave an engine running.
#[derive(Debug)]
pub(super) enum RegistryError {
    /// The company failed recently and its backoff has not elapsed. Not an
    /// error of this message: a refusal to rebuild a broken company once
    /// per message.
    Quarantined { tenant: String, retry_in: Duration },
    /// The build itself was refused.
    Build { tenant: String, detail: String },
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryError::Quarantined { tenant, retry_in } => write!(
                f,
                "tenant {tenant} is quarantined after a failure; not retried for another {}s",
                retry_in.as_secs()
            ),
            RegistryError::Build { tenant, detail } => {
                write!(f, "tenant {tenant} failed to start: {detail}")
            }
        }
    }
}

impl std::error::Error for RegistryError {}

impl<R: RunTenant> TenantRegistry<R> {
    /// What one company's failure costs the shard (plan B5).
    ///
    /// A pure decision over the error and the failures already seen: no
    /// store, no engine, no channel. Exhaustive over `EngineError` with no
    /// wildcard arm, for the reason `TurnFailure::verdict` has none - a
    /// fourth variant must not inherit a policy by accident.
    pub(super) fn on_tenant_error(&self, tenant: &str, e: &EngineError) -> ShardVerdict {
        match e {
            // One company's database file. Every company here has its own,
            // so one failing says nothing about the others - until several
            // of them fail inside the window, which is the disk.
            EngineError::Store(_) => {
                let now = Instant::now();
                let mut seen = locked(&self.store_errors);
                seen.retain(|(_, at)| now.duration_since(*at) < self.limits.store_window);
                if !seen.iter().any(|(id, _)| id == tenant) {
                    seen.push((tenant.to_string(), now));
                }
                if seen.len() >= self.limits.store_fatal_tenants {
                    let tenants: Vec<String> = seen.iter().map(|(id, _)| id.clone()).collect();
                    ShardVerdict::ShardFatal(ShardFatal {
                        reason: format!(
                            "store failures from {} tenants within {}s",
                            tenants.len(),
                            self.limits.store_window.as_secs()
                        ),
                        tenants,
                    })
                } else {
                    ShardVerdict::TenantOnly
                }
            }
            // A metered run reaching its ceiling. Not a fault, and one
            // company's arrangement.
            EngineError::RequestCap { .. } => ShardVerdict::TenantOnly,
            // One company's connection went away.
            EngineError::Channel(_) => ShardVerdict::TenantOnly,
        }
    }

    /// How long until this company may be rebuilt, or `None` if it may now.
    pub(super) fn quarantined_for(&self, tenant: &str) -> Option<Duration> {
        let held = locked(&self.quarantine);
        let q = held.get(tenant)?;
        q.until.checked_duration_since(Instant::now())
    }

    /// What a run that ended leaves behind.
    pub(super) fn finish(&self, tenant: &str, outcome: Result<(), EngineError>) {
        if let Err(e) = outcome {
            let stats = self.stats_for(tenant);
            stats.failures.fetch_add(1, Ordering::SeqCst);
            // The engine's own request count, attributed to the company
            // that spent it.
            if let EngineError::RequestCap { spent, .. } = &e {
                stats.record_spend(*spent);
            }
            match self.on_tenant_error(tenant, &e) {
                ShardVerdict::TenantOnly => {
                    let retry_in = self.quarantine(tenant);
                    eprintln!(
                        "tenant {tenant} stopped: {e}; not retried for {}s",
                        retry_in.as_secs()
                    );
                }
                ShardVerdict::ShardFatal(fatal) => {
                    eprintln!("shard stopping: {fatal}");
                    *locked(&self.fatal) = Some(fatal);
                    self.fatal_notify.notify_one();
                }
            }
        }
        self.unregister(tenant);
    }

    /// Quarantine this company and say for how long.
    pub(super) fn quarantine(&self, tenant: &str) -> Duration {
        let mut held = locked(&self.quarantine);
        let entry = held.entry(tenant.to_string()).or_insert(Quarantine {
            attempts: 0,
            until: Instant::now(),
        });
        entry.attempts = entry.attempts.saturating_add(1);
        let wait = backoff_for(
            entry.attempts,
            self.limits.backoff_base,
            self.limits.backoff_max,
        );
        entry.until = Instant::now() + wait;
        wait
    }
}

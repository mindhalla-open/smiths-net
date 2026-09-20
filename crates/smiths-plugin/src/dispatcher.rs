//! Hook dispatcher — priority-ordered fan-out with a shared budget.
//!
//! A [`Hook`] is a single named action one plugin exposes. A
//! [`Dispatcher`] routes a named *event* to every hook registered for
//! it, in priority order (low numbers first). The dispatcher enforces
//! a per-event time budget so a single slow hook cannot hold up the
//! rest: a hook that overruns the remaining budget is cut off and the
//! hooks after it are skipped, each reported as `Err`.
//!
//! [`MemoryDispatcher`] is the implementation the engine uses. The
//! [`crate::AiRegistry`] owns one; the loader registers every hook a
//! manifest declares (`hooks = [...]`, ordered by `priority`) and the
//! call-event consumer ([`crate::spawn_call_event_hooks`]) dispatches
//! SIP dialog events through it. Hooks are tracked per owning plugin
//! so a reload or removal drops exactly that plugin's registrations.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dashmap::DashMap;
use serde_json::Value;
use tokio::time::timeout;
use tracing::{debug, warn};

/// One registered hook. Named so reports stay human-readable.
#[async_trait]
pub trait Hook: Send + Sync {
    /// Identifier for logs / reports — typically `"plugin.method"`.
    fn name(&self) -> &str;
    /// Invoke with a JSON payload; return a JSON result or an error
    /// message. Errors are **not** fatal for the dispatch as a whole.
    async fn invoke(&self, payload: Value) -> Result<Value, String>;
}

/// Outcome of one hook invocation inside a [`Dispatcher::dispatch`]
/// fan-out.
#[derive(Debug)]
pub struct HookReport {
    /// Hook name ([`Hook::name`]).
    pub hook: String,
    /// `Ok(value)` on success, `Err(reason)` when the hook returned an
    /// error, timed out, or was skipped because the budget was spent.
    pub outcome: Result<Value, String>,
    /// Wall-clock spent inside `invoke` (or near-zero for skipped).
    pub duration: Duration,
}

/// Priority-ordered hook fan-out.
#[async_trait]
pub trait Dispatcher: Send + Sync {
    /// Register `hook` for the named `event` at the given `priority`
    /// (lower runs first). Duplicate `(event, name)` pairs overwrite.
    fn register(&self, event: &str, priority: u16, hook: Arc<dyn Hook>);
    /// Fan out `payload` to every hook registered for `event`. Hooks
    /// run sequentially (simpler to reason about than racing them);
    /// a hook exceeding the remaining per-event budget is skipped with
    /// `Err("budget exhausted")`.
    async fn dispatch(&self, event: &str, payload: Value) -> Vec<HookReport>;
}

/// One entry on an event's route list.
#[derive(Clone)]
struct Route {
    priority: u16,
    /// Who registered the hook — a plugin name for manifest hooks —
    /// so [`MemoryDispatcher::unregister_owner`] can drop them.
    owner: String,
    hook: Arc<dyn Hook>,
}

/// In-memory implementation. Cheap to clone (`Arc<DashMap>` inside).
#[derive(Clone)]
pub struct MemoryDispatcher {
    /// `event` → routes sorted by priority (stable, so equal
    /// priorities keep registration order).
    routes: Arc<DashMap<String, Vec<Route>>>,
    /// Per-dispatch time budget. `None` = no cap.
    budget: Option<Duration>,
}

impl MemoryDispatcher {
    /// Build a dispatcher with no per-event budget.
    #[must_use]
    pub fn new() -> Self {
        Self {
            routes: Arc::new(DashMap::new()),
            budget: None,
        }
    }

    /// Build a dispatcher that caps each `dispatch` call's total wall
    /// clock at `budget`.
    #[must_use]
    pub fn with_budget(budget: Duration) -> Self {
        Self {
            routes: Arc::new(DashMap::new()),
            budget: Some(budget),
        }
    }

    /// Per-dispatch budget, if any.
    #[must_use]
    pub fn budget(&self) -> Option<Duration> {
        self.budget
    }

    /// Register `hook` for `event` on behalf of `owner`. Same
    /// replace-on-duplicate semantics as [`Dispatcher::register`]
    /// (keyed on the hook name), plus the owner tag that
    /// [`Self::unregister_owner`] uses.
    pub fn register_owned(&self, owner: &str, event: &str, priority: u16, hook: Arc<dyn Hook>) {
        let mut entry = self.routes.entry(event.to_owned()).or_default();
        let name = hook.name().to_owned();
        entry.retain(|r| r.hook.name() != name);
        entry.push(Route {
            priority,
            owner: owner.to_owned(),
            hook,
        });
        entry.sort_by_key(|r| r.priority);
    }

    /// Drop every hook `owner` registered, across all events.
    pub fn unregister_owner(&self, owner: &str) {
        for mut entry in self.routes.iter_mut() {
            entry.retain(|r| r.owner != owner);
        }
    }

    /// Drop every route.
    pub fn clear(&self) {
        self.routes.clear();
    }

    /// Hook names registered for `event`, in dispatch order.
    #[must_use]
    pub fn hooks_for(&self, event: &str) -> Vec<String> {
        self.routes
            .get(event)
            .map(|e| e.iter().map(|r| r.hook.name().to_owned()).collect())
            .unwrap_or_default()
    }

    /// Total number of registered hooks across all events.
    #[must_use]
    pub fn len(&self) -> usize {
        self.routes.iter().map(|e| e.value().len()).sum()
    }

    /// `true` if no routes are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.routes.iter().all(|e| e.value().is_empty())
    }
}

impl Default for MemoryDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Dispatcher for MemoryDispatcher {
    fn register(&self, event: &str, priority: u16, hook: Arc<dyn Hook>) {
        let owner = hook.name().to_owned();
        self.register_owned(&owner, event, priority, hook);
    }

    async fn dispatch(&self, event: &str, payload: Value) -> Vec<HookReport> {
        let hooks: Vec<Route> = self
            .routes
            .get(event)
            .map(|e| e.value().clone())
            .unwrap_or_default();
        if hooks.is_empty() {
            debug!(event, "dispatch: no hooks");
            return Vec::new();
        }

        let start = Instant::now();
        let mut reports = Vec::with_capacity(hooks.len());
        for route in hooks {
            let hook = route.hook;
            let name = hook.name().to_owned();
            let remaining = self
                .budget
                .map(|total| total.saturating_sub(start.elapsed()));
            if matches!(remaining, Some(r) if r.is_zero()) {
                warn!(event, hook = %name, "skipped: per-event budget exhausted");
                reports.push(HookReport {
                    hook: name,
                    outcome: Err("budget exhausted".into()),
                    duration: Duration::ZERO,
                });
                continue;
            }
            let hook_start = Instant::now();
            let outcome = match remaining {
                Some(r) => match timeout(r, hook.invoke(payload.clone())).await {
                    Ok(res) => res,
                    Err(_) => Err("budget exhausted".into()),
                },
                None => hook.invoke(payload.clone()).await,
            };
            reports.push(HookReport {
                hook: name,
                outcome,
                duration: hook_start.elapsed(),
            });
        }
        reports
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Recorder {
        name: &'static str,
        calls: Arc<Mutex<Vec<&'static str>>>,
        delay: Duration,
        reply: Result<Value, String>,
    }

    #[async_trait]
    impl Hook for Recorder {
        fn name(&self) -> &str {
            self.name
        }
        async fn invoke(&self, _payload: Value) -> Result<Value, String> {
            self.calls.lock().unwrap().push(self.name);
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            self.reply.clone()
        }
    }

    fn hook(name: &'static str, calls: Arc<Mutex<Vec<&'static str>>>) -> Arc<dyn Hook> {
        Arc::new(Recorder {
            name,
            calls,
            delay: Duration::ZERO,
            reply: Ok(Value::Null),
        })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dispatch_respects_priority() {
        let dispatcher = MemoryDispatcher::new();
        let calls = Arc::new(Mutex::new(Vec::new()));
        dispatcher.register("on_invite", 20, hook("b", Arc::clone(&calls)));
        dispatcher.register("on_invite", 10, hook("a", Arc::clone(&calls)));
        dispatcher.register("on_invite", 30, hook("c", Arc::clone(&calls)));

        let reports = dispatcher.dispatch("on_invite", Value::Null).await;
        assert_eq!(reports.len(), 3);
        let order: Vec<&str> = reports.iter().map(|r| r.hook.as_str()).collect();
        assert_eq!(order, vec!["a", "b", "c"]);
        assert_eq!(*calls.lock().unwrap(), vec!["a", "b", "c"]);
        assert_eq!(dispatcher.hooks_for("on_invite"), vec!["a", "b", "c"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_hooks_returns_empty() {
        let dispatcher = MemoryDispatcher::new();
        let reports = dispatcher.dispatch("missing", Value::Null).await;
        assert!(reports.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn per_event_budget_skips_slow_tail() {
        let dispatcher = MemoryDispatcher::with_budget(Duration::from_millis(100));
        let calls = Arc::new(Mutex::new(Vec::new()));
        dispatcher.register(
            "on_rtp",
            10,
            Arc::new(Recorder {
                name: "slow",
                calls: Arc::clone(&calls),
                delay: Duration::from_millis(200),
                reply: Ok(Value::Null),
            }),
        );
        dispatcher.register("on_rtp", 20, hook("fast", Arc::clone(&calls)));

        let reports = dispatcher.dispatch("on_rtp", Value::Null).await;
        assert_eq!(reports.len(), 2);
        // `slow` times out; `fast` is skipped because budget is gone.
        assert_eq!(reports[0].hook, "slow");
        assert!(reports[0].outcome.is_err());
        assert_eq!(reports[1].hook, "fast");
        assert!(matches!(reports[1].outcome, Err(ref m) if m == "budget exhausted"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn re_register_replaces_previous() {
        let dispatcher = MemoryDispatcher::new();
        let calls = Arc::new(Mutex::new(Vec::new()));
        dispatcher.register("e", 50, hook("same", Arc::clone(&calls)));
        dispatcher.register("e", 10, hook("same", Arc::clone(&calls)));
        let reports = dispatcher.dispatch("e", Value::Null).await;
        assert_eq!(reports.len(), 1); // only the second registration remained
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unregister_owner_drops_only_that_owner_across_events() {
        let dispatcher = MemoryDispatcher::new();
        let calls = Arc::new(Mutex::new(Vec::new()));
        dispatcher.register_owned("p1", "created", 10, hook("p1.created", Arc::clone(&calls)));
        dispatcher.register_owned("p1", "ended", 10, hook("p1.ended", Arc::clone(&calls)));
        dispatcher.register_owned("p2", "created", 20, hook("p2.created", Arc::clone(&calls)));
        assert_eq!(dispatcher.len(), 3);

        dispatcher.unregister_owner("p1");
        assert_eq!(dispatcher.len(), 1);
        assert_eq!(dispatcher.hooks_for("created"), vec!["p2.created"]);
        assert!(dispatcher.hooks_for("ended").is_empty());
    }
}

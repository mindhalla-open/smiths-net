//! Embedded DSL host for script-tier plugins.
//!
//! Today a Rhai-only implementation; the shape is engine-agnostic so
//! Lua / Starlark can slot in behind [`ScriptEngineKind`] without the
//! caller (plugin loader, MCP tool host) changing. The runtime owns
//! the `rhai::Engine` + compiled AST, enforces op-count and wall-
//! clock budgets at every invocation, and surfaces a uniform
//! JSON-in / JSON-out contract so Rhai provides the same method
//! interface sidecars + WASM guests do.
//!
//! ## Contract
//!
//! Every script is expected to define at least one function:
//!
//! ```rhai
//! // Returns a capability descriptor (or an array of them) shaped
//! // like `describe_capabilities` returns from the plugin protocol.
//! fn describe_capabilities { ... }
//! ```
//!
//! Every other exported function is a capability method. The engine
//! calls `engine.call_fn(scope, ast, "<method>", (json_params,))`
//! when a consumer invokes, passing the params as a Rhai `Map` /
//! `Array` built from the request JSON and converting the result
//! back to `serde_json::Value` on the way out.
//!
//! ## Budgets
//!
//! * **Op-count**: Rhai's built-in `limits::set_max_operations`.
//!   Guards against runaway loops; the default is 1M ops/invoke.
//! * **Wall-clock**: each `call_fn` runs on `spawn_blocking` (Rhai is
//!   synchronous) under a `tokio::time::timeout`. The timeout alone
//!   would only abandon the blocking task, so the runtime also
//!   installs a Rhai progress callback that *stops the script*: it
//!   checks a stop flag the timed-out caller raises and a deadline
//!   the script armed for itself when it started. Either trips the
//!   script with a `Budget` error and releases the engine for the
//!   next call. Default 500 ms — tight enough to keep a dialplan
//!   script from blocking a worker thread, loose enough to allow
//!   string-munging + lookup-table work.
//!
//! Budgets are public ([`ScriptLimits`]) so the plugin loader can
//! lower them further via manifest.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rhai::{AST, Dynamic, Engine, Map, Scope};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::Mutex;

/// Which DSL engine a script uses. Today: Rhai only. Lua /
/// Starlark slot in here without changing the [`ScriptRuntime`] trait
/// surface.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ScriptEngineKind {
    /// Rhai 1.x.
    #[default]
    Rhai,
}

impl std::fmt::Display for ScriptEngineKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rhai => f.write_str("rhai"),
        }
    }
}

/// Per-invocation resource caps. The engine enforces both; whichever
/// trips first surfaces as a descriptive error.
#[derive(Clone, Copy, Debug)]
pub struct ScriptLimits {
    /// Maximum Rhai operations per call. Defaults to 1M.
    pub max_operations: u64,
    /// Maximum wall-clock per call. Defaults to 500 ms.
    pub wall_clock: Duration,
}

impl Default for ScriptLimits {
    fn default() -> Self {
        Self {
            max_operations: 1_000_000,
            wall_clock: Duration::from_millis(500),
        }
    }
}

/// Every failure path a script can take.
#[derive(Debug, Error)]
pub enum ScriptError {
    /// Script source failed to compile.
    #[error("compile `{path}`: {reason}")]
    Compile {
        /// File path the engine was compiling.
        path: String,
        /// Underlying Rhai compiler message.
        reason: String,
    },
    /// A called method returned an error.
    #[error("script call `{method}`: {reason}")]
    Call {
        /// Method name the caller invoked.
        method: String,
        /// Underlying Rhai runtime message.
        reason: String,
    },
    /// Wall-clock or op-count budget exceeded.
    #[error("budget exceeded: {reason}")]
    Budget {
        /// Which budget tripped.
        reason: String,
    },
    /// Declared capability method isn't exported by the script.
    #[error("method `{method}` not defined in script")]
    MethodMissing {
        /// Method name the caller invoked.
        method: String,
    },
    /// Input / output JSON couldn't be translated to Rhai types.
    #[error("json <-> rhai bridge: {0}")]
    Bridge(String),
}

/// A compiled, callable script. Cloneable — inner state is
/// reference-counted. Construct via [`ScriptRuntime::load_rhai`].
#[derive(Clone)]
pub struct ScriptRuntime {
    name: String,
    kind: ScriptEngineKind,
    limits: ScriptLimits,
    path: PathBuf,
    /// Wrapped in `Arc<Mutex<..>>` because Rhai's `Engine` + `AST`
    /// are `!Send` without the `sync` feature, and even with `sync`
    /// the engine still serializes calls. One script = one execution
    /// lane; multiple concurrent calls queue behind the mutex.
    inner: Arc<Mutex<RhaiInner>>,
    /// Stop-flag + deadline shared with the engine's progress
    /// callback so a timed-out call actually terminates the script.
    interrupt: Arc<Interrupt>,
}

struct RhaiInner {
    engine: Engine,
    ast: AST,
}

/// How often (in Rhai operations) the progress callback re-reads the
/// clock for the in-script deadline. The stop flag is checked on
/// every operation — it is two relaxed atomic loads — but
/// `Instant::now` is not free, so the deadline is sampled.
const DEADLINE_PROBE_EVERY_OPS: u64 = 256;

/// Cooperative-interruption state shared between the async caller
/// and the blocking thread running the Rhai engine.
///
/// Every call gets a unique generation number. The blocking side
/// publishes the generation it is executing (under the engine
/// mutex, so at most one call is active) plus its deadline; a caller
/// whose timeout fired stores its own generation into `stop`. The
/// progress callback terminates the script when `stop` matches the
/// active generation — a stale stop request from an earlier call can
/// never hit a later one because generations are never reused.
struct Interrupt {
    /// Source of unique call generations.
    seq: AtomicU64,
    /// Generation currently executing inside the engine; `0` = idle.
    active: AtomicU64,
    /// Generation that was asked to stop; `0` = none.
    stop: AtomicU64,
    /// Deadline of the active call as nanoseconds since `base`;
    /// `0` = no deadline armed.
    deadline_nanos: AtomicU64,
    /// Reference point for `deadline_nanos`.
    base: Instant,
}

impl Interrupt {
    fn new() -> Self {
        Self {
            seq: AtomicU64::new(0),
            active: AtomicU64::new(0),
            stop: AtomicU64::new(0),
            deadline_nanos: AtomicU64::new(0),
            base: Instant::now(),
        }
    }

    /// Hand out the next call generation (never `0`).
    fn next_generation(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Mark `generation` as executing with a deadline `wall_clock`
    /// from now. Called on the blocking thread once the engine mutex
    /// is held.
    fn begin(&self, generation: u64, wall_clock: Duration) {
        let deadline = Instant::now() + wall_clock;
        let nanos = u64::try_from(deadline.saturating_duration_since(self.base).as_nanos())
            .unwrap_or(u64::MAX)
            .max(1);
        self.deadline_nanos.store(nanos, Ordering::Release);
        self.active.store(generation, Ordering::Release);
    }

    /// Clear the active generation once the call returned.
    fn end(&self) {
        self.active.store(0, Ordering::Release);
        self.deadline_nanos.store(0, Ordering::Release);
    }

    /// Ask the script running as `generation` to stop. No-op if that
    /// generation already finished or hasn't started yet — in the
    /// latter case the deadline armed by `begin` still bounds it.
    fn request_stop(&self, generation: u64) {
        self.stop.store(generation, Ordering::Release);
    }

    /// Progress-callback probe: `true` when the active script must
    /// terminate now.
    fn should_stop(&self, ops: u64) -> bool {
        let active = self.active.load(Ordering::Acquire);
        if active != 0 && self.stop.load(Ordering::Acquire) == active {
            return true;
        }
        if !ops.is_multiple_of(DEADLINE_PROBE_EVERY_OPS) {
            return false;
        }
        let deadline = self.deadline_nanos.load(Ordering::Acquire);
        if deadline == 0 {
            return false;
        }
        let now = u64::try_from(self.base.elapsed().as_nanos()).unwrap_or(u64::MAX);
        now >= deadline
    }
}

impl std::fmt::Debug for ScriptRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScriptRuntime")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("path", &self.path)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl ScriptRuntime {
    /// Compile a Rhai script at `path` and return a ready-to-call
    /// runtime. `name` is the plugin name (for diagnostics /
    /// errors).
    pub fn load_rhai(
        name: impl Into<String>,
        path: impl Into<PathBuf>,
        limits: ScriptLimits,
    ) -> Result<Self, ScriptError> {
        let path = path.into();
        let source = std::fs::read_to_string(&path).map_err(|e| ScriptError::Compile {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        Self::from_source_rhai(name, source, path, limits)
    }

    /// Compile Rhai `source` directly. Used by `put_script` so agents
    /// don't have to round-trip through the filesystem for a
    /// one-shot ephemeral script.
    #[allow(clippy::needless_pass_by_value)]
    pub fn from_source_rhai(
        name: impl Into<String>,
        source: String,
        path: impl Into<PathBuf>,
        limits: ScriptLimits,
    ) -> Result<Self, ScriptError> {
        let path = path.into();
        let mut engine = Engine::new();
        engine.set_max_operations(limits.max_operations);
        // Cooperative interruption: Rhai invokes this on every
        // operation; returning `Some` terminates the script with
        // `ErrorTerminated`, which `call_rhai` maps to a `Budget`
        // error. This is what makes the wall-clock budget release
        // the engine instead of merely abandoning the blocking task.
        let interrupt = Arc::new(Interrupt::new());
        let probe = Arc::clone(&interrupt);
        engine.on_progress(move |ops| probe.should_stop(ops).then_some(Dynamic::UNIT));
        let ast = engine.compile(&source).map_err(|e| ScriptError::Compile {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        Ok(Self {
            name: name.into(),
            kind: ScriptEngineKind::Rhai,
            limits,
            path,
            inner: Arc::new(Mutex::new(RhaiInner { engine, ast })),
            interrupt,
        })
    }

    /// Plugin name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Which DSL the script targets.
    #[must_use]
    pub fn kind(&self) -> ScriptEngineKind {
        self.kind
    }

    /// Compiled source path (for reload / diagnostics).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Call `describe_capabilities`. Every script must define it;
    /// the result is expected to be either a single object or an
    /// array of objects shaped like the plugin-protocol capability
    /// descriptor (caller parses through
    /// `smiths_core::ai::parse_descriptors`).
    pub async fn describe(&self) -> Result<Value, ScriptError> {
        self.call("describe_capabilities", Value::Null).await
    }

    /// Invoke `method(params)`. Returns the JSON value the script
    /// produced, or a [`ScriptError`] if the script errored, the
    /// method isn't defined, or the op-count / wall-clock budget
    /// tripped.
    ///
    /// On a wall-clock timeout the caller gets `Budget` immediately
    /// *and* the script is interrupted through the engine's progress
    /// callback, so the blocking worker (and the engine mutex) are
    /// released within a few operations instead of running until the
    /// op-count budget is exhausted.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, ScriptError> {
        let method_str = method.to_owned();
        let inner = Arc::clone(&self.inner);
        let interrupt = Arc::clone(&self.interrupt);
        let wall_clock = self.limits.wall_clock;
        let generation = self.interrupt.next_generation();
        let run = async move {
            tokio::task::spawn_blocking(move || {
                let guard = inner.blocking_lock();
                interrupt.begin(generation, wall_clock);
                let out = call_rhai(&guard.engine, &guard.ast, &method_str, &params);
                interrupt.end();
                out
            })
            .await
            .map_err(|e| ScriptError::Call {
                method: method.to_owned(),
                reason: format!("blocking worker panicked: {e}"),
            })?
        };
        if let Ok(res) = tokio::time::timeout(wall_clock, run).await {
            return res;
        }
        self.interrupt.request_stop(generation);
        Err(ScriptError::Budget {
            reason: format!(
                "wall-clock timeout ({} ms) for method `{method}`",
                wall_clock.as_millis()
            ),
        })
    }

    /// Engine-agnostic convenience — call
    /// [`Self::call`] under whichever engine this runtime uses.
    /// Exists so consumers can call `runtime.call_any(...)` without
    /// matching on [`ScriptEngineKind`] today and keep working when
    /// Lua/Starlark land.
    pub async fn call_any(&self, method: &str, params: Value) -> Result<Value, ScriptError> {
        self.call(method, params).await
    }
}

fn call_rhai(
    engine: &Engine,
    ast: &AST,
    method: &str,
    params: &Value,
) -> Result<Value, ScriptError> {
    // Rhai doesn't treat arity mismatch as "function not found" — it
    // errors with a specific ArityMismatch. We peek at the AST first
    // to both (a) turn missing-function into the more helpful
    // `MethodMissing` variant and (b) pick whether to pass the
    // params arg at all (nullary `describe_capabilities` is the
    // idiomatic shape; every other method takes one `req` arg).
    let arity = ast
        .iter_functions()
        .find(|f| f.name == method)
        .map(|f| f.params.len())
        .ok_or_else(|| ScriptError::MethodMissing {
            method: method.to_owned(),
        })?;

    let mut scope = Scope::new();
    let map_err = |e: Box<rhai::EvalAltResult>| match *e {
        rhai::EvalAltResult::ErrorTooManyOperations(_) => ScriptError::Budget {
            reason: "op-count exceeded".into(),
        },
        rhai::EvalAltResult::ErrorTerminated(..) => ScriptError::Budget {
            reason: "wall-clock deadline reached; script interrupted".into(),
        },
        _ => ScriptError::Call {
            method: method.to_owned(),
            reason: e.to_string(),
        },
    };

    let result: Dynamic = if arity == 0 {
        engine
            .call_fn(&mut scope, ast, method, ())
            .map_err(map_err)?
    } else {
        let arg = json_to_rhai(params)?;
        engine
            .call_fn(&mut scope, ast, method, (arg,))
            .map_err(map_err)?
    };
    rhai_to_json(&result)
}

/// Translate a `serde_json::Value` into a Rhai `Dynamic`. Rhai's
/// `serde_json` integration exists behind a feature flag we don't
/// enable by default; this hand-rolled conversion keeps the crate
/// small and avoids dragging in their JSON dep chain.
fn json_to_rhai(v: &Value) -> Result<Dynamic, ScriptError> {
    Ok(match v {
        Value::Null => Dynamic::UNIT,
        Value::Bool(b) => Dynamic::from_bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Dynamic::from_int(i)
            } else if let Some(f) = n.as_f64() {
                Dynamic::from_float(f)
            } else {
                return Err(ScriptError::Bridge(format!(
                    "unrepresentable JSON number: {n}"
                )));
            }
        }
        Value::String(s) => Dynamic::from(s.clone()),
        Value::Array(a) => {
            let mut arr = rhai::Array::new();
            for item in a {
                arr.push(json_to_rhai(item)?);
            }
            Dynamic::from_array(arr)
        }
        Value::Object(o) => {
            let mut map = Map::new();
            for (k, val) in o {
                map.insert(k.as_str().into(), json_to_rhai(val)?);
            }
            Dynamic::from_map(map)
        }
    })
}

fn rhai_to_json(v: &Dynamic) -> Result<Value, ScriptError> {
    if v.is_unit() {
        return Ok(Value::Null);
    }
    if let Some(b) = v.clone().try_cast::<bool>() {
        return Ok(Value::Bool(b));
    }
    if let Some(i) = v.clone().try_cast::<i64>() {
        return Ok(Value::Number(i.into()));
    }
    if let Some(f) = v.clone().try_cast::<f64>() {
        return serde_json::Number::from_f64(f)
            .map(Value::Number)
            .ok_or_else(|| ScriptError::Bridge(format!("non-finite float from script: {f}")));
    }
    if let Some(s) = v.clone().try_cast::<String>() {
        return Ok(Value::String(s));
    }
    if let Some(arr) = v.clone().try_cast::<rhai::Array>() {
        let mut out = Vec::with_capacity(arr.len());
        for item in &arr {
            out.push(rhai_to_json(item)?);
        }
        return Ok(Value::Array(out));
    }
    if let Some(map) = v.clone().try_cast::<Map>() {
        let mut obj = serde_json::Map::new();
        for (k, val) in &map {
            obj.insert(k.to_string(), rhai_to_json(val)?);
        }
        return Ok(Value::Object(obj));
    }
    Err(ScriptError::Bridge(format!(
        "unsupported rhai return type: {}",
        v.type_name()
    )))
}

/// Async adapter trait so plugin-layer code can hold a
/// `Box<dyn ScriptEngine>` behind a uniform API as Lua / Starlark
/// variants land. Today it just delegates to [`ScriptRuntime`].
#[async_trait]
pub trait ScriptEngine: Send + Sync + 'static {
    /// Name of the plugin this engine hosts.
    fn name(&self) -> &str;
    /// Which engine implementation is in use.
    fn kind(&self) -> ScriptEngineKind;
    /// Call `describe_capabilities` and return its JSON value.
    async fn describe(&self) -> Result<Value, ScriptError>;
    /// Call an arbitrary method with JSON params.
    async fn invoke(&self, method: &str, params: Value) -> Result<Value, ScriptError>;
}

#[async_trait]
impl ScriptEngine for ScriptRuntime {
    fn name(&self) -> &str {
        Self::name(self)
    }
    fn kind(&self) -> ScriptEngineKind {
        Self::kind(self)
    }
    async fn describe(&self) -> Result<Value, ScriptError> {
        Self::describe(self).await
    }
    async fn invoke(&self, method: &str, params: Value) -> Result<Value, ScriptError> {
        Self::call(self, method, params).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn runtime(source: &str) -> ScriptRuntime {
        ScriptRuntime::from_source_rhai(
            "test",
            source.to_owned(),
            PathBuf::from("test.rhai"),
            ScriptLimits::default(),
        )
        .expect("compile ok")
    }

    #[tokio::test]
    async fn describe_returns_declared_descriptor() {
        let r = runtime(
            r#"
            fn describe_capabilities() {
                [#{
                    "capability": "routing",
                    "plugin": "test",
                    "abi": "1.0",
                }]
            }
            "#,
        );
        let v = r.describe().await.unwrap();
        assert_eq!(v[0]["capability"], "routing");
    }

    #[tokio::test]
    async fn call_round_trips_json_inputs() {
        let r = runtime(
            r#"
            fn route(req) {
                let to = req["to"];
                #{ "target": `sip:${to}@pbx.internal`, "score": 42 }
            }
            "#,
        );
        let out = r.call("route", json!({"to": "alice"})).await.unwrap();
        assert_eq!(out["target"], "sip:alice@pbx.internal");
        assert_eq!(out["score"], 42);
    }

    #[tokio::test]
    async fn missing_method_is_a_clean_error() {
        let r = runtime("fn describe_capabilities() { #{} }");
        let err = r.call("nope", Value::Null).await.unwrap_err();
        assert!(matches!(err, ScriptError::MethodMissing { .. }));
    }

    #[tokio::test]
    async fn op_budget_trips_on_runaway_loop() {
        let r = ScriptRuntime::from_source_rhai(
            "runaway",
            "fn run(req) { let n = 0; loop { n += 1; } }".into(),
            PathBuf::from("runaway.rhai"),
            ScriptLimits {
                max_operations: 500,
                wall_clock: Duration::from_secs(5),
            },
        )
        .unwrap();
        let err = r.call("run", Value::Null).await.unwrap_err();
        assert!(
            matches!(err, ScriptError::Budget { .. }),
            "expected Budget, got {err:?}"
        );
    }

    /// A runaway script with an effectively unlimited op budget must
    /// be stopped by the wall-clock budget — not merely abandoned —
    /// so the engine is free for the next call.
    #[tokio::test(flavor = "multi_thread")]
    async fn wall_clock_timeout_interrupts_the_running_script() {
        let wall_clock = Duration::from_millis(100);
        let r = ScriptRuntime::from_source_rhai(
            "spinner",
            "fn spin(req) { let n = 0; loop { n += 1; } }\nfn fast(req) { 7 }".into(),
            PathBuf::from("spinner.rhai"),
            ScriptLimits {
                max_operations: u64::MAX,
                wall_clock,
            },
        )
        .unwrap();

        let started = Instant::now();
        let err = r.call("spin", Value::Null).await.unwrap_err();
        assert!(
            matches!(err, ScriptError::Budget { .. }),
            "expected Budget, got {err:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "timeout must fire promptly; took {:?}",
            started.elapsed()
        );

        // The spinner held the engine mutex. If it were still running,
        // this call would queue behind it and hit its own 100 ms
        // budget. Interruption frees the lane, so it completes.
        let started = Instant::now();
        let out = r.call("fast", Value::Null).await.unwrap();
        assert_eq!(out, json!(7));
        assert!(
            started.elapsed() < wall_clock,
            "follow-up call should not queue behind the interrupted script; took {:?}",
            started.elapsed()
        );
    }

    /// A stop request raised for an earlier (timed-out) call must
    /// not leak into a later call on the same runtime.
    #[tokio::test(flavor = "multi_thread")]
    async fn stale_stop_request_does_not_kill_the_next_call() {
        let r = ScriptRuntime::from_source_rhai(
            "gen",
            "fn spin(req) { let n = 0; loop { n += 1; } }\n\
             fn work(req) { let n = 0; for i in 0..5000 { n += i; } n }"
                .into(),
            PathBuf::from("gen.rhai"),
            ScriptLimits {
                max_operations: u64::MAX,
                wall_clock: Duration::from_millis(80),
            },
        )
        .unwrap();
        let err = r.call("spin", Value::Null).await.unwrap_err();
        assert!(matches!(err, ScriptError::Budget { .. }));
        // Thousands of operations after the stale stop request — must
        // run to completion.
        let out = r.call("work", Value::Null).await.unwrap();
        assert_eq!(out, json!(12_497_500));
    }

    #[tokio::test]
    async fn compile_error_surfaces_cleanly() {
        let err = ScriptRuntime::from_source_rhai(
            "bad",
            "fn ( { } }".into(),
            PathBuf::from("bad.rhai"),
            ScriptLimits::default(),
        )
        .unwrap_err();
        assert!(matches!(err, ScriptError::Compile { .. }));
    }

    #[test]
    fn json_rhai_round_trip() {
        let input = json!({
            "a": 1,
            "b": "x",
            "c": [1, 2, 3],
            "d": { "nested": true }
        });
        let dy = json_to_rhai(&input).unwrap();
        let back = rhai_to_json(&dy).unwrap();
        assert_eq!(input, back);
    }
}

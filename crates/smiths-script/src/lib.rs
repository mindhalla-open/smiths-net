//! Embedded DSL host for script-tier plugins (slice 4.1 / P24).
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
//! fn describe_capabilities() { ... }
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
//! * **Wall-clock**: the runtime wraps each `call_fn` in a
//!   `tokio::time::timeout` run on `spawn_blocking` since Rhai is
//!   synchronous. Default 500 ms — tight enough to keep a dialplan
//!   script from blocking a worker thread, loose enough to allow
//!   string-munging + lookup-table work.
//!
//! Budgets are public ([`ScriptLimits`]) so the plugin loader can
//! lower them further via manifest.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

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
}

struct RhaiInner {
    engine: Engine,
    ast: AST,
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

    /// Call `describe_capabilities()`. Every script must define it;
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
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, ScriptError> {
        let method_str = method.to_owned();
        let inner = Arc::clone(&self.inner);
        let wall_clock = self.limits.wall_clock;
        let run = async move {
            tokio::task::spawn_blocking(move || {
                let guard = inner.blocking_lock();
                call_rhai(&guard.engine, &guard.ast, &method_str, &params)
            })
            .await
            .map_err(|e| ScriptError::Call {
                method: method.to_owned(),
                reason: format!("blocking worker panicked: {e}"),
            })?
        };
        match tokio::time::timeout(wall_clock, run).await {
            Ok(res) => res,
            Err(_) => Err(ScriptError::Budget {
                reason: format!(
                    "wall-clock timeout ({} ms) for method `{method}`",
                    wall_clock.as_millis()
                ),
            }),
        }
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
    // params arg at all (nullary `describe_capabilities()` is the
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

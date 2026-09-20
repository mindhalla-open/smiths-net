//! Adapter-agnostic tool abstraction.
//!
//! A [`Tool`] is one named operation the engine exposes on its control
//! plane. Both the MCP and A2A adapters register the same
//! [`ToolRegistry`] and invoke tools through it. The wire protocol of
//! the adapter has no say in what the tool does or what it returns;
//! that keeps the tool set small and testable independent of any
//! transport.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use prometheus_client::registry::Registry;
use serde_json::Value;
use smiths_core::Config;
use smiths_core::ConfigReloader;
use smiths_core::Metrics;
use smiths_core::ai::{AiDispatcher, AiRegistry};
use smiths_core::call::{CallOriginator, RegistrationView};
use smiths_core::media::MediaFabric;
use smiths_core::storage::{CdrStore, RecordingStore, VectorStore};
use smiths_media::PromptLibrary;
use smiths_mixer::ConferenceRegistry;
use thiserror::Error;
use tokio::sync::Mutex;

use crate::cluster::ClusterStatusSource;
use crate::control::ControlState;

/// Context passed to every tool invocation.
#[derive(Clone)]
pub struct ToolContext {
    /// Live engine state, updated by the event-bus subscription.
    pub state: ControlState,
    /// Loaded AI plugins behind the core trait seam. Empty registry
    /// when no plugins dir is configured or when every plugin failed
    /// to load — tools that depend on it should return a clean error
    /// in that case.
    pub plugins: Arc<dyn AiRegistry>,
    /// Engine configuration snapshot. Resources like `config://current`
    /// read from this; tools that need a config knob copy it into their
    /// arg schema rather than reaching through here.
    pub config: Arc<Config>,
    /// Media plane handle. Tools that inject or forward audio
    /// (`speak`, future `listen`) reach in here; pure signaling /
    /// stateful tools ignore it.
    pub media: Arc<dyn MediaFabric>,
    /// Outbound-call originator. `None` when the engine is configured
    /// UAS-only — tools that depend on it (`make_call`, `end_call`)
    /// return a clean `NotFound` instead of panicking.
    pub originator: Option<Arc<dyn CallOriginator>>,
    /// Read-only view over the subscriber DB's live registrations
    ///. `None` when no backend is wired (e.g.
    /// `[auth] backend = "none"`); `sip://registrations` returns an
    /// empty snapshot in that case rather than 404-ing.
    pub registrations: Option<Arc<dyn RegistrationView>>,
    /// CDR store. `None` when `[storage] backend =
    /// "none"`; `list_cdr` returns an empty page in that case.
    pub cdr: Option<Arc<dyn CdrStore>>,
    /// Engine-wide Prometheus metrics. `None` only in old test
    /// fixtures; tools that observe histograms (the
    /// pipeline tools) fall back to `Metrics::noop` when absent
    /// so the observe call still goes somewhere reasonable.
    pub metrics: Option<Arc<Metrics>>,
    /// Vector store. `None` when `[storage.vector]
    /// backend = "none"`; `search_calls_semantic` returns a clean
    /// `NotFound` in that case.
    pub vector: Option<Arc<dyn VectorStore>>,
    /// Recording store. `None` when `[storage.recording]
    /// backend = "none"`; `transcribe_call` / `summarize_call` then
    /// require the `audio_base64` argument as before.
    pub recording: Option<Arc<dyn RecordingStore>>,
    /// IVR prompt library. `None` when the operator
    /// hasn't configured a root; `record_prompt` returns a clean
    /// `NotFound` in that case.
    pub prompts: Option<PromptLibrary>,
    /// Conference registry. `None` when no mixer fabric
    /// is wired; `create_conference` / `join_conference` /
    /// `leave_conference` return a clean `NotFound` in that case.
    pub conferences: Option<Arc<dyn ConferenceRegistry>>,
    /// Shared Prometheus registry. Wired by the CLI
    /// from the same `Arc<Mutex<Registry>>` the `/metrics`
    /// endpoint encodes from, so `list_metrics` / `get_metric`
    /// MCP tools can never disagree with a scrape. `None` in
    /// tests and in transport-free contexts.
    pub metrics_registry: Option<Arc<Mutex<Registry>>>,
    /// Config hot-reload handle. `None` in test contexts;
    /// `put_config` returns `NotFound` without it.
    pub reloader: Option<Arc<ConfigReloader>>,
    /// Filesystem path the engine loaded its config from.
    /// Used by `put_config(persist=true)` to write back to the same file.
    pub config_path: Option<PathBuf>,
    /// In-memory ring buffer of `put_config` calls.
    /// Shared between the `put_config` tool and the `config://history`
    /// resource.
    pub config_history: Option<crate::config_history::ConfigHistory>,
    /// Live HA status provider backing `cluster://status`. `None`
    /// makes the resource report `status: "unverified"`.
    pub cluster_status: Option<Arc<dyn ClusterStatusSource>>,
    /// AI dispatcher over `plugins`, built once so its per-provider
    /// health / breaker state persists across tool calls instead of
    /// being reset on every invocation.
    pub dispatcher: Arc<AiDispatcher>,
}

impl ToolContext {
    /// Build a context from its components.
    #[must_use]
    pub fn new(
        state: ControlState,
        plugins: Arc<dyn AiRegistry>,
        config: Arc<Config>,
        media: Arc<dyn MediaFabric>,
    ) -> Self {
        let dispatcher = Arc::new(AiDispatcher::new(Arc::clone(&plugins)));
        Self {
            state,
            plugins,
            config,
            media,
            dispatcher,
            originator: None,
            registrations: None,
            cdr: None,
            metrics: None,
            vector: None,
            recording: None,
            prompts: None,
            conferences: None,
            metrics_registry: None,
            reloader: None,
            config_path: None,
            config_history: None,
            cluster_status: None,
        }
    }

    /// Attach a [`ClusterStatusSource`] so `cluster://status` reports
    /// the live HA role and peer state instead of `unverified`.
    #[must_use]
    pub fn with_cluster_status(mut self, source: Arc<dyn ClusterStatusSource>) -> Self {
        self.cluster_status = Some(source);
        self
    }

    /// Attach a [`ConferenceRegistry`] so the conferencing MCP tools
    /// become live. Without it they return a clean `NotFound`.
    #[must_use]
    pub fn with_conferences(mut self, registry: Arc<dyn ConferenceRegistry>) -> Self {
        self.conferences = Some(registry);
        self
    }

    /// Attach the shared Prometheus `Registry` so `list_metrics` /
    /// `get_metric` become live.
    #[must_use]
    pub fn with_metrics_registry(mut self, registry: Arc<Mutex<Registry>>) -> Self {
        self.metrics_registry = Some(registry);
        self
    }

    /// Attach a [`PromptLibrary`] so `record_prompt` can write
    /// prompts to disk + cache decoded WAVs for IVR playback.
    #[must_use]
    pub fn with_prompts(mut self, library: PromptLibrary) -> Self {
        self.prompts = Some(library);
        self
    }

    /// Attach a [`VectorStore`] so `search_calls_semantic` is live.
    /// Without one the tool returns a clean `NotFound` naming the
    /// config knob the operator hasn't flipped.
    #[must_use]
    pub fn with_vector(mut self, store: Arc<dyn VectorStore>) -> Self {
        self.vector = Some(store);
        self
    }

    /// Attach a [`RecordingStore`] so the pipeline tools can
    /// resolve audio from a bare `call_id`.
    #[must_use]
    pub fn with_recording(mut self, store: Arc<dyn RecordingStore>) -> Self {
        self.recording = Some(store);
        self
    }

    /// Attach the engine's metrics handle so pipeline tools can
    /// observe their histograms and the shared AI dispatcher counts
    /// invocations / fail-overs. Without it they fall through to a
    /// scratch `Metrics::noop` so no code path panics.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.dispatcher = Arc::new(
            AiDispatcher::new(Arc::clone(&self.plugins)).with_metrics(Arc::clone(&metrics)),
        );
        self.metrics = Some(metrics);
        self
    }

    /// Attach a [`CallOriginator`] so `make_call` / `end_call` tools
    /// become live.
    #[must_use]
    pub fn with_originator(mut self, originator: Arc<dyn CallOriginator>) -> Self {
        self.originator = Some(originator);
        self
    }

    /// Attach a [`RegistrationView`] so the `sip://registrations`
    /// resource can render live subscriber bindings. Without it the
    /// resource returns an empty snapshot — callers can still
    /// differentiate "registrar disabled" from "registered but idle"
    /// by checking `[auth] backend` in `config://current`.
    #[must_use]
    pub fn with_registrations(mut self, view: Arc<dyn RegistrationView>) -> Self {
        self.registrations = Some(view);
        self
    }

    /// Attach a [`CdrStore`] so `list_cdr` returns live data. Without
    /// it the tool returns `{count: 0, rows: []}` — callers can
    /// tell the difference by reading `[storage] backend` from
    /// `config://current`.
    #[must_use]
    pub fn with_cdr(mut self, store: Arc<dyn CdrStore>) -> Self {
        self.cdr = Some(store);
        self
    }

    /// Attach the [`ConfigReloader`] so `put_config` can drive live
    /// config changes through the same `apply` path SIGHUP uses.
    #[must_use]
    pub fn with_reloader(mut self, reloader: Arc<ConfigReloader>) -> Self {
        self.reloader = Some(reloader);
        self
    }

    /// Store the config file path so `put_config(persist=true)` can
    /// write back to the same TOML file the engine booted from.
    #[must_use]
    pub fn with_config_path(mut self, path: PathBuf) -> Self {
        self.config_path = Some(path);
        self
    }
}

/// Errors a tool can return to the adapter layer.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ToolError {
    /// Input JSON didn't conform to the tool's declared schema.
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),
    /// The caller referenced something that doesn't exist (e.g. unknown call id).
    #[error("not found: {0}")]
    NotFound(String),
    /// The caller was denied: the shared rate limiter rejected the
    /// call, or an adapter-level authorization check failed.
    #[error("forbidden: {0}")]
    Forbidden(String),
    /// The referent exists but is in a state that rejects the
    /// operation (e.g. bridging a call that carries no media).
    #[error("conflict: {0}")]
    Conflict(String),
    /// Unexpected internal failure.
    #[error("internal: {0}")]
    Internal(String),
}

/// One typed control-plane operation.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Unique, stable identifier used on the wire (e.g. `list_calls`).
    fn name(&self) -> &'static str;

    /// One-sentence human description exposed to MCP clients.
    fn description(&self) -> &'static str;

    /// JSON Schema (draft 2020-12) describing the expected `arguments`
    /// object. Empty schema = no arguments.
    fn input_schema(&self) -> Value;

    /// Execute. The dispatcher has already checked `args` against
    /// [`Self::input_schema`] (required fields, primitive types,
    /// enums); implementations still own semantic validation such as
    /// ranges and cross-field rules.
    async fn call(&self, args: Value, ctx: &ToolContext) -> Result<Value, ToolError>;
}

/// Ordered collection of tools, keyed by name.
#[derive(Default)]
pub struct ToolRegistry {
    // BTreeMap so `list` output is deterministic.
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    /// Empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool; duplicate names overwrite the previous entry.
    pub fn register<T: Tool + 'static>(&mut self, tool: T) {
        self.tools.insert(tool.name().to_owned(), Arc::new(tool));
    }

    /// Look up a tool by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// Iterate over every registered tool in name order.
    pub fn iter(&self) -> impl Iterator<Item = Arc<dyn Tool>> + '_ {
        self.tools.values().cloned()
    }

    /// Number of registered tools.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// `true` if no tools are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Test doubles shared by the crate's unit tests: an empty
    //! [`AiRegistry`], a static registry of canned providers, a
    //! [`MediaFabric`] that errors on every operation, and one that
    //! records every operation for assertions.

    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_trait::async_trait;
    use serde_json::{Value, json};
    use smiths_core::ai::{AiProvider, AiRegistry, CapabilityDescriptor, ProviderError};
    use smiths_core::media::{
        BridgeId, BridgeLeg, EndpointId, MediaEndpoint, MediaError, MediaFabric,
    };
    use smiths_core::{Config, Event, EventBus, SipEvent};
    use tokio_util::sync::CancellationToken;

    use super::ToolContext;
    use crate::control::ControlState;

    /// Registry with no providers. Every lookup returns `None`.
    pub(crate) struct EmptyRegistry;

    #[async_trait]
    impl AiRegistry for EmptyRegistry {
        fn get(&self, _name: &str) -> Option<Arc<dyn AiProvider>> {
            None
        }
        fn snapshot(&self) -> Vec<Arc<dyn AiProvider>> {
            Vec::new()
        }
        async fn shutdown_all(&self) {}
    }

    /// Registry over a fixed provider list.
    pub(crate) struct StaticRegistry(pub(crate) Vec<Arc<dyn AiProvider>>);

    #[async_trait]
    impl AiRegistry for StaticRegistry {
        fn get(&self, name: &str) -> Option<Arc<dyn AiProvider>> {
            self.0.iter().find(|p| p.name() == name).cloned()
        }
        fn snapshot(&self) -> Vec<Arc<dyn AiProvider>> {
            self.0.clone()
        }
        async fn shutdown_all(&self) {}
    }

    /// Provider that answers every `invoke` with a canned value and
    /// records the `(method, params)` pairs it saw.
    pub(crate) struct FakeProvider {
        name: String,
        capabilities: Vec<CapabilityDescriptor>,
        response: Value,
        pub(crate) calls: Mutex<Vec<(String, Value)>>,
    }

    impl FakeProvider {
        /// A provider named `name` advertising `capability` with the
        /// given `extra` descriptor fields (voices, controls,...).
        pub(crate) fn new(
            name: &str,
            capability: &str,
            extra: &Value,
            response: Value,
        ) -> Arc<Self> {
            let mut descriptor: CapabilityDescriptor = serde_json::from_value(json!({
                "capability": capability,
                "plugin": name,
                "model_id": "fake",
            }))
            .expect("descriptor");
            if let Some(obj) = extra.as_object() {
                for (k, v) in obj {
                    descriptor.extra.insert(k.clone(), v.clone());
                }
            }
            Arc::new(Self {
                name: name.to_owned(),
                capabilities: vec![descriptor],
                response,
                calls: Mutex::new(Vec::new()),
            })
        }

        pub(crate) fn last_call(&self) -> Option<(String, Value)> {
            self.calls.lock().unwrap().last().cloned()
        }
    }

    #[async_trait]
    impl AiProvider for FakeProvider {
        fn name(&self) -> &str {
            &self.name
        }
        fn version(&self) -> &'static str {
            "0.0.1"
        }
        fn description(&self) -> &'static str {
            "fake provider"
        }
        fn abi(&self) -> &'static str {
            "1.0"
        }
        fn capabilities(&self) -> &[CapabilityDescriptor] {
            &self.capabilities
        }
        async fn invoke(&self, method: &str, params: Value) -> Result<Value, ProviderError> {
            self.calls.lock().unwrap().push((method.to_owned(), params));
            Ok(self.response.clone())
        }
    }

    /// `MediaFabric` that errors on every operation. Every test that
    /// exercises the signaling / tool surface but doesn't touch
    /// media passes this in.
    pub(crate) struct NullMedia;

    #[async_trait]
    impl MediaFabric for NullMedia {
        async fn allocate(
            &self,
            _: std::net::IpAddr,
        ) -> Result<Arc<dyn MediaEndpoint>, MediaError> {
            Err(MediaError::PortExhausted("null fabric".into()))
        }
        async fn bridge(&self, _: BridgeLeg, _: BridgeLeg) -> Result<BridgeId, MediaError> {
            Err(MediaError::PortExhausted("null fabric".into()))
        }
        async fn release_bridge(&self, _: BridgeId) {}
        async fn release_endpoint(&self, _: EndpointId) {}
        async fn send_packet(
            &self,
            _: EndpointId,
            _: SocketAddr,
            _: &[u8],
        ) -> Result<(), MediaError> {
            Err(MediaError::PortExhausted("null fabric".into()))
        }
    }

    /// One packet the [`FakeFabric`] was asked to send.
    #[derive(Clone, Debug)]
    pub(crate) struct SentPacket {
        pub(crate) src: EndpointId,
        pub(crate) dest: SocketAddr,
        pub(crate) bytes: Vec<u8>,
    }

    /// `MediaFabric` that records bridges, releases, and packets so
    /// tests can assert on the media-plane side effects of a tool.
    #[derive(Default)]
    pub(crate) struct FakeFabric {
        next_bridge: AtomicU64,
        pub(crate) bridges: Mutex<Vec<(BridgeId, BridgeLeg, BridgeLeg)>>,
        pub(crate) released: Mutex<Vec<BridgeId>>,
        pub(crate) sent: Mutex<Vec<SentPacket>>,
    }

    impl FakeFabric {
        pub(crate) fn sent(&self) -> Vec<SentPacket> {
            self.sent.lock().unwrap().clone()
        }
        pub(crate) fn released(&self) -> Vec<BridgeId> {
            self.released.lock().unwrap().clone()
        }
        pub(crate) fn bridges(&self) -> Vec<(BridgeId, BridgeLeg, BridgeLeg)> {
            self.bridges.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl MediaFabric for FakeFabric {
        async fn allocate(
            &self,
            _: std::net::IpAddr,
        ) -> Result<Arc<dyn MediaEndpoint>, MediaError> {
            Err(MediaError::PortExhausted("fake fabric".into()))
        }
        async fn bridge(&self, a: BridgeLeg, b: BridgeLeg) -> Result<BridgeId, MediaError> {
            let id = BridgeId(self.next_bridge.fetch_add(1, Ordering::Relaxed) + 100);
            self.bridges.lock().unwrap().push((id, a, b));
            Ok(id)
        }
        async fn release_bridge(&self, id: BridgeId) {
            self.released.lock().unwrap().push(id);
        }
        async fn release_endpoint(&self, _: EndpointId) {}
        async fn send_packet(
            &self,
            src: EndpointId,
            dest: SocketAddr,
            bytes: &[u8],
        ) -> Result<(), MediaError> {
            self.sent.lock().unwrap().push(SentPacket {
                src,
                dest,
                bytes: bytes.to_vec(),
            });
            Ok(())
        }
    }

    /// Convenience: build an `Arc<dyn AiRegistry>` holding an [`EmptyRegistry`].
    pub(crate) fn empty_registry() -> Arc<dyn AiRegistry> {
        Arc::new(EmptyRegistry)
    }

    /// Default-config [`Arc<Config>`] for test contexts.
    pub(crate) fn default_config() -> Arc<Config> {
        Arc::new(Config::default())
    }

    /// `Arc<dyn MediaFabric>` that errors on every op — for tests that
    /// don't exercise media.
    pub(crate) fn null_media() -> Arc<dyn MediaFabric> {
        Arc::new(NullMedia)
    }

    /// Everything a tool test needs: a context whose state drains
    /// `bus`, plus the cancel token that stops the drain task.
    pub(crate) struct TestEngine {
        pub(crate) ctx: ToolContext,
        pub(crate) bus: EventBus,
        pub(crate) fabric: Arc<FakeFabric>,
        pub(crate) cancel: CancellationToken,
    }

    impl TestEngine {
        /// Context over an empty AI registry and a recording fabric.
        pub(crate) fn new() -> Self {
            Self::with_registry(empty_registry())
        }

        /// Context over `registry` and a recording fabric.
        pub(crate) fn with_registry(registry: Arc<dyn AiRegistry>) -> Self {
            let bus = EventBus::new(32);
            let cancel = CancellationToken::new();
            let (state, _task) = ControlState::spawn(&bus, cancel.clone());
            let fabric = Arc::new(FakeFabric::default());
            let media: Arc<dyn MediaFabric> = Arc::clone(&fabric) as Arc<dyn MediaFabric>;
            let ctx = ToolContext::new(state, registry, default_config(), media);
            Self {
                ctx,
                bus,
                fabric,
                cancel,
            }
        }

        /// Publish `DialogCreated` for `call_id` with an optional
        /// media leg and wait for the control state to absorb it.
        pub(crate) async fn dialog_created(
            &self,
            call_id: &str,
            leg: Option<(EndpointId, SocketAddr)>,
        ) {
            self.bus
                .publish(Event::Sip(SipEvent::DialogCreated {
                    call_id: call_id.to_owned(),
                    media_endpoint: leg.map(|l| l.0),
                    remote_rtp: leg.map(|l| l.1),
                }))
                .expect("bus has a subscriber");
            self.wait_for_call(call_id).await;
        }

        /// Publish `DialogTerminated` for `call_id` and wait for it
        /// to land.
        pub(crate) async fn dialog_terminated(&self, call_id: &str) {
            self.bus
                .publish(Event::Sip(SipEvent::DialogTerminated {
                    call_id: call_id.to_owned(),
                }))
                .expect("bus has a subscriber");
            for _ in 0..100 {
                if self
                    .ctx
                    .state
                    .get_call(call_id)
                    .is_some_and(|c| c.ended_at.is_some())
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            panic!("dialog {call_id} never terminated in control state");
        }

        async fn wait_for_call(&self, call_id: &str) {
            for _ in 0..100 {
                if self.ctx.state.get_call(call_id).is_some() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            panic!("dialog {call_id} never reached control state");
        }
    }

    impl Drop for TestEngine {
        fn drop(&mut self) {
            self.cancel.cancel();
        }
    }
}

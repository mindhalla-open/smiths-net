//! Walking-skeleton WASM engine tests.
//!
//! Uses inline WebAssembly Text (WAT) so the tests don't need a
//! cross-compile toolchain. Scenarios:
//!
//! 1. Guest calls the `smiths::log` host fn and returns.
//! 2. Guest hits `unreachable`: the engine reports a trap and the
//!    process survives.
//! 3. Guest spins forever: fuel runs out and the engine reports
//!    `FuelExhausted`.

// Test fakes use `unimplemented!()` / `unreachable!()` in methods the
// scenario doesn't exercise — that's the idiomatic "shouldn't be hit"
// marker. The workspace lint forbids them in production code; this
// crate-local allow keeps test fakes terse.
#![allow(clippy::unimplemented)]

use smiths_wasm::{WasmEngine, WasmError};

const FUEL: u64 = 1_000_000;

fn compile(engine: &WasmEngine, wat: &str) -> wasmtime::Module {
    let wasm = wat::parse_str(wat).expect("WAT parse");
    engine.load(&wasm).expect("compile")
}

#[test]
fn guest_calls_host_log_and_returns_ok() {
    let wat = r#"
(module
  (import "smiths" "log" (func $log (param i32 i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "hello from plugin")
  (func (export "run")
    i32.const 0
    i32.const 17
    call $log))
"#;
    let engine = WasmEngine::new().unwrap();
    let module = compile(&engine, wat);
    engine
        .run_entry(&module, "run", FUEL, "inline-wat")
        .expect("run_entry");
}

#[test]
fn trap_does_not_crash_engine_and_second_module_still_runs() {
    let trap_wat = r#"
(module
  (memory (export "memory") 1)
  (func (export "run")
    unreachable))
"#;
    let ok_wat = r#"
(module
  (memory (export "memory") 1)
  (func (export "run")
    nop))
"#;
    let engine = WasmEngine::new().unwrap();

    let trap_module = compile(&engine, trap_wat);
    let err = engine
        .run_entry(&trap_module, "run", FUEL, "trap-plugin")
        .unwrap_err();
    assert!(
        matches!(err, WasmError::Trap(_)),
        "expected Trap, got {err:?}"
    );

    // Engine is still usable — the trap isolated to the previous
    // instance's store.
    let ok_module = compile(&engine, ok_wat);
    engine
        .run_entry(&ok_module, "run", FUEL, "ok-plugin")
        .expect("second run");
}

#[test]
fn fuel_exhaustion_is_reported_cleanly() {
    // Tight infinite loop — will burn through any finite fuel budget.
    let wat = r#"
(module
  (memory (export "memory") 1)
  (func (export "run")
    (loop $L br $L)))
"#;
    let engine = WasmEngine::new().unwrap();
    let module = compile(&engine, wat);
    let err = engine
        .run_entry(&module, "run", 10_000, "spinner")
        .unwrap_err();
    assert!(
        matches!(err, WasmError::FuelExhausted { fuel } if fuel == 10_000),
        "expected FuelExhausted, got {err:?}"
    );
}

#[test]
fn missing_entry_is_reported() {
    let wat = r#"
(module
  (memory (export "memory") 1)
  (func (export "something_else")
    nop))
"#;
    let engine = WasmEngine::new().unwrap();
    let module = compile(&engine, wat);
    let err = engine
        .run_entry(&module, "run", FUEL, "no-entry")
        .unwrap_err();
    assert!(
        matches!(err, WasmError::MissingExport(ref name) if name == "run"),
        "expected MissingExport(\"run\"), got {err:?}"
    );
}

#[test]
fn wall_clock_deadline_traps_infinite_loop_even_with_unlimited_fuel() {
    // Same tight-loop guest, but we give it *more* fuel than it will
    // ever burn inside the deadline window. Only the epoch timer can
    // stop it.
    let wat = r#"
(module
  (memory (export "memory") 1)
  (func (export "run")
    (loop $L br $L)))
"#;
    let engine = WasmEngine::new().unwrap();
    let module = compile(&engine, wat);
    let err = engine
        .run_with_deadline(
            &module,
            "run",
            u64::MAX / 2,
            "slow",
            std::time::Duration::from_millis(80),
        )
        .unwrap_err();
    assert!(
        matches!(err, WasmError::Timeout { millis } if millis >= 80),
        "expected Timeout, got {err:?}"
    );
}

#[test]
fn run_with_deadline_returns_ok_when_guest_completes_in_time() {
    let wat = r#"
(module
  (memory (export "memory") 1)
  (func (export "run") nop))
"#;
    let engine = WasmEngine::new().unwrap();
    let module = compile(&engine, wat);
    engine
        .run_with_deadline(
            &module,
            "run",
            FUEL,
            "quick",
            std::time::Duration::from_secs(2),
        )
        .expect("quick guest shouldn't trip the deadline");
}

#[test]
fn state_set_then_get_round_trips_within_one_guest_call() {
    // Guest writes "v" under key "k" via state_set, then reads it
    // back via state_get and asserts the length matches (2 for "v" →
    // wait, our value is 2 chars; let's just pick a known value).
    // We store 3 bytes under a 1-byte key, then read them back into
    // a 16-byte buffer, trap if the returned length differs from 3.
    let wat = r#"
(module
  (import "smiths" "state_set" (func $set (param i32 i32 i32 i32) (result i32)))
  (import "smiths" "state_get" (func $get (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "k")            ;; key at 0..1
  (data (i32.const 8) "abc")          ;; value at 8..11
  (func (export "run")
    ;; state_set(key=0,1, val=8,3)
    (call $set (i32.const 0) (i32.const 1) (i32.const 8) (i32.const 3))
    drop
    ;; state_get(key=0,1, out=32, cap=16) → expected length = 3
    (call $get (i32.const 0) (i32.const 1) (i32.const 32) (i32.const 16))
    i32.const 3
    i32.ne
    (if (then unreachable))))
"#;
    let engine = WasmEngine::new().unwrap();
    engine.set_plugin_permissions("state-smoke", ["state"]);
    let module = compile(&engine, wat);
    engine
        .run_entry(&module, "run", FUEL, "state-smoke")
        .expect("round-trip should succeed");
}

#[test]
fn state_persists_across_two_run_entry_calls() {
    // First module populates state. Second module reads it and
    // returns normally only if the length matches.
    let writer = r#"
(module
  (import "smiths" "state_set" (func $set (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "k")
  (data (i32.const 8) "abcdefgh")     ;; 8-byte value
  (func (export "run")
    (call $set (i32.const 0) (i32.const 1) (i32.const 8) (i32.const 8))
    drop))
"#;
    let reader = r#"
(module
  (import "smiths" "state_get" (func $get (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "k")
  (func (export "run")
    (call $get (i32.const 0) (i32.const 1) (i32.const 32) (i32.const 64))
    i32.const 8
    i32.ne
    (if (then unreachable))))
"#;
    let engine = WasmEngine::new().unwrap();
    engine.set_plugin_permissions("persist-demo", ["state"]);
    let w = compile(&engine, writer);
    let r = compile(&engine, reader);
    engine.run_entry(&w, "run", FUEL, "persist-demo").unwrap();
    engine
        .run_entry(&r, "run", FUEL, "persist-demo")
        .expect("state should persist across Stores");
}

#[test]
fn state_is_namespaced_per_plugin() {
    let writer = r#"
(module
  (import "smiths" "state_set" (func $set (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "k")
  (data (i32.const 8) "xxx")
  (func (export "run")
    (call $set (i32.const 0) (i32.const 1) (i32.const 8) (i32.const 3))
    drop))
"#;
    let reader = r#"
(module
  (import "smiths" "state_get" (func $get (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "k")
  (func (export "run")
    (call $get (i32.const 0) (i32.const 1) (i32.const 32) (i32.const 16))
    i32.const -1
    i32.ne
    (if (then unreachable))))
"#;
    let engine = WasmEngine::new().unwrap();
    engine.set_plugin_permissions("plugin-a", ["state"]);
    engine.set_plugin_permissions("plugin-b", ["state"]);
    let w = compile(&engine, writer);
    let r = compile(&engine, reader);
    // Plugin A writes.
    engine.run_entry(&w, "run", FUEL, "plugin-a").unwrap();
    // Plugin B reads the same key — must see -1 (not found).
    engine
        .run_entry(&r, "run", FUEL, "plugin-b")
        .expect("plugin-b's lookup should miss");
}

#[test]
fn log_out_of_bounds_is_a_trap() {
    // ptr=0, len=1024 but memory is only 64 KB of zero bytes — still
    // in-bounds. To force OOB we ask for a len that exceeds one page.
    let wat = r#"
(module
  (import "smiths" "log" (func $log (param i32 i32)))
  (memory (export "memory") 1)
  (func (export "run")
    i32.const 0
    i32.const 131072
    call $log))
"#;
    let engine = WasmEngine::new().unwrap();
    let module = compile(&engine, wat);
    let err = engine.run_entry(&module, "run", FUEL, "oob").unwrap_err();
    assert!(
        matches!(err, WasmError::Trap(_)),
        "expected Trap, got {err:?}"
    );
}

#[test]
fn state_set_without_permission_traps_with_permission_denied() {
    // Same write path as state_set_then_get_round_trips, but without
    // registering the "state" permission. The engine must surface a
    // typed PermissionDenied rather than a bare Trap so operators can
    // distinguish "plugin bug" from "plugin over-reach".
    let wat = r#"
(module
  (import "smiths" "state_set" (func $set (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "k")
  (data (i32.const 8) "v")
  (func (export "run")
    (call $set (i32.const 0) (i32.const 1) (i32.const 8) (i32.const 1))
    drop))
"#;
    let engine = WasmEngine::new().unwrap();
    let module = compile(&engine, wat);
    let err = engine
        .run_entry(&module, "run", FUEL, "unprivileged")
        .unwrap_err();
    match err {
        WasmError::PermissionDenied {
            plugin,
            permission,
            op,
        } => {
            assert_eq!(plugin, "unprivileged");
            assert_eq!(permission, "state");
            assert_eq!(op, "state_set");
        }
        other => panic!("expected PermissionDenied, got {other:?}"),
    }
}

#[test]
fn call_invoke_round_trips_result_envelope() {
    // Minimal guest: memory-export, bump alloc, and an `invoke` that
    // always returns `{"result":"ok"}`. Exercises every step of the
    // invoke ABI trampoline host-side.
    let wat = r#"
(module
  (memory (export "memory") 1)
  ;; Response envelope at offset 0, 15 bytes.
  (data (i32.const 0) "{\"result\":\"ok\"}")
  ;; Bump allocator starts at 512 (well past the envelope).
  (global $HEAP (mut i32) (i32.const 512))
  (func (export "alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $HEAP))
    (global.set $HEAP (i32.add (global.get $HEAP) (local.get $len)))
    (local.get $ptr))
  (func (export "invoke") (param i32 i32 i32 i32) (result i64)
    ;; (0 << 32) | 15
    i64.const 15))
"#;
    let engine = WasmEngine::new().unwrap();
    let module = compile(&engine, wat);
    let result = engine
        .call_invoke(&module, "invoke-demo", "ping", &serde_json::json!({"x": 1}))
        .expect("invoke should round-trip");
    assert_eq!(result, serde_json::Value::String("ok".into()));
}

#[tokio::test(flavor = "multi_thread")]
async fn publish_event_reaches_bus_subscriber() {
    use smiths_core::{Event, EventBus, PluginEvent};
    let wat = r#"
(module
  (import "smiths" "publish_event" (func $pub (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "chat")        ;; topic at 0..4
  (data (i32.const 16) "hello")      ;; payload at 16..21
  (func (export "run")
    (call $pub (i32.const 0) (i32.const 4) (i32.const 16) (i32.const 5))
    drop))
"#;
    let bus = EventBus::new(16);
    let mut rx = bus.subscribe();
    let engine = WasmEngine::new().unwrap().with_bus(bus);
    engine.set_plugin_permissions("pub-plugin", ["events"]);
    let module = compile(&engine, wat);
    engine
        .run_entry(&module, "run", FUEL, "pub-plugin")
        .unwrap();

    let event = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
        .await
        .expect("bus should have seen the event")
        .expect("bus recv");
    match event {
        Event::Plugin(PluginEvent::Published {
            plugin,
            topic,
            data,
        }) => {
            assert_eq!(plugin, "pub-plugin");
            assert_eq!(topic, "chat");
            assert_eq!(&data, b"hello");
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[test]
fn publish_event_without_permission_traps() {
    let wat = r#"
(module
  (import "smiths" "publish_event" (func $pub (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "t")
  (data (i32.const 8) "x")
  (func (export "run")
    (call $pub (i32.const 0) (i32.const 1) (i32.const 8) (i32.const 1))
    drop))
"#;
    let bus = smiths_core::EventBus::new(4);
    let engine = WasmEngine::new().unwrap().with_bus(bus);
    // No permissions registered → events is denied.
    let module = compile(&engine, wat);
    let err = engine
        .run_entry(&module, "run", FUEL, "no-perm")
        .unwrap_err();
    assert!(
        matches!(err, WasmError::PermissionDenied { ref permission, .. } if permission == "events"),
        "expected PermissionDenied(events), got {err:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn timer_set_fires_timer_event_after_delay() {
    use smiths_core::{Event, EventBus, PluginEvent};
    let wat = r#"
(module
  (import "smiths" "timer_set" (func $timer (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (func (export "run")
    (call $timer (i32.const 40) (i32.const 42))
    drop))
"#;
    let bus = EventBus::new(16);
    let mut rx = bus.subscribe();
    let engine = WasmEngine::new().unwrap().with_bus(bus);
    engine.set_plugin_permissions("timer-plugin", ["timers"]);
    let module = compile(&engine, wat);
    engine
        .run_entry(&module, "run", FUEL, "timer-plugin")
        .unwrap();

    // Wait up to 500 ms for the timer thread to fire back on the bus.
    let event = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
        .await
        .expect("timer should fire within 500 ms")
        .expect("bus recv");
    match event {
        Event::Plugin(PluginEvent::TimerFired { plugin, event_id }) => {
            assert_eq!(plugin, "timer-plugin");
            assert_eq!(event_id, 42);
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn send_rtp_dispatches_through_media_fabric() {
    use async_trait::async_trait;
    use smiths_core::CallLookup;
    use smiths_core::media::{BridgeId, EndpointId, MediaEndpoint, MediaError, MediaFabric};
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::Mutex;

    // Stub fabric that records send_packet calls instead of touching
    // UDP. Lets us verify the engine dispatched correctly without a
    // real socket.
    type SendLog = Arc<Mutex<Vec<(EndpointId, SocketAddr, Vec<u8>)>>>;

    #[derive(Default)]
    struct RecordingFabric {
        sends: SendLog,
    }
    #[async_trait]
    impl MediaFabric for RecordingFabric {
        async fn allocate(
            &self,
            _: std::net::IpAddr,
        ) -> Result<Arc<dyn MediaEndpoint>, MediaError> {
            unimplemented!()
        }
        async fn bridge(
            &self,
            _: smiths_core::BridgeLeg,
            _: smiths_core::BridgeLeg,
        ) -> Result<BridgeId, MediaError> {
            unimplemented!()
        }
        async fn release_bridge(&self, _: BridgeId) {}
        async fn release_endpoint(&self, _: EndpointId) {}
        async fn send_packet(
            &self,
            src: EndpointId,
            dest: SocketAddr,
            bytes: &[u8],
        ) -> Result<(), MediaError> {
            self.sends.lock().unwrap().push((src, dest, bytes.to_vec()));
            Ok(())
        }
    }

    // Stub CallLookup: only knows one call-id.
    struct FixedLookup {
        call_id: String,
        endpoint: EndpointId,
        remote: SocketAddr,
    }
    impl CallLookup for FixedLookup {
        fn endpoint_for(&self, call_id: &str) -> Option<(EndpointId, SocketAddr)> {
            (call_id == self.call_id).then_some((self.endpoint, self.remote))
        }
    }

    let wat = r#"
(module
  (import "smiths" "send_rtp" (func $send (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  ;; call-id at 0..7 ("call-42"), payload at 16..20 (4 bytes)
  (data (i32.const 0) "call-42")
  (data (i32.const 16) "\01\02\03\04")
  (func (export "run")
    (call $send (i32.const 0) (i32.const 7) (i32.const 16) (i32.const 4))
    drop))
"#;
    let fabric = Arc::new(RecordingFabric::default());
    let sends = Arc::clone(&fabric.sends);
    let lookup = Arc::new(FixedLookup {
        call_id: "call-42".into(),
        endpoint: EndpointId(7),
        remote: "127.0.0.1:9999".parse().unwrap(),
    });

    let engine = WasmEngine::new()
        .unwrap()
        .with_media(lookup, fabric as Arc<dyn MediaFabric>);
    engine.set_plugin_permissions("rtp-demo", ["send_rtp"]);
    let module = compile(&engine, wat);
    engine
        .run_entry(&module, "run", FUEL, "rtp-demo")
        .expect("run_entry");

    // The spawn is fire-and-forget — yield to let it land.
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        if !sends.lock().unwrap().is_empty() {
            break;
        }
    }
    let recorded = sends.lock().unwrap().clone();
    assert_eq!(recorded.len(), 1);
    let (ep, dest, bytes) = &recorded[0];
    assert_eq!(*ep, EndpointId(7));
    assert_eq!(dest.port(), 9999);
    assert_eq!(bytes.as_slice(), &[1, 2, 3, 4]);
}

#[tokio::test(flavor = "multi_thread")]
async fn send_rtp_without_permission_traps() {
    use async_trait::async_trait;
    use smiths_core::CallLookup;
    use smiths_core::media::{BridgeId, EndpointId, MediaEndpoint, MediaError, MediaFabric};
    use std::net::SocketAddr;
    use std::sync::Arc;

    struct NoopFabric;
    #[async_trait]
    impl MediaFabric for NoopFabric {
        async fn allocate(
            &self,
            _: std::net::IpAddr,
        ) -> Result<Arc<dyn MediaEndpoint>, MediaError> {
            unimplemented!()
        }
        async fn bridge(
            &self,
            _: smiths_core::BridgeLeg,
            _: smiths_core::BridgeLeg,
        ) -> Result<BridgeId, MediaError> {
            unimplemented!()
        }
        async fn release_bridge(&self, _: BridgeId) {}
        async fn release_endpoint(&self, _: EndpointId) {}
        async fn send_packet(
            &self,
            _: EndpointId,
            _: SocketAddr,
            _: &[u8],
        ) -> Result<(), MediaError> {
            Ok(())
        }
    }
    struct EmptyLookup;
    impl CallLookup for EmptyLookup {
        fn endpoint_for(&self, _: &str) -> Option<(EndpointId, SocketAddr)> {
            None
        }
    }

    let wat = r#"
(module
  (import "smiths" "send_rtp" (func $send (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "x")
  (func (export "run")
    (call $send (i32.const 0) (i32.const 1) (i32.const 0) (i32.const 1))
    drop))
"#;
    let engine = WasmEngine::new()
        .unwrap()
        .with_media(Arc::new(EmptyLookup), Arc::new(NoopFabric));
    // No permission registered.
    let module = compile(&engine, wat);
    let err = engine
        .run_entry(&module, "run", FUEL, "no-perm")
        .unwrap_err();
    assert!(
        matches!(err, WasmError::PermissionDenied { ref permission, .. } if permission == "send_rtp"),
        "expected PermissionDenied(send_rtp), got {err:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn originate_dispatches_through_call_originator() {
    use async_trait::async_trait;
    use smiths_core::call::{CallError, CallOriginator};
    use std::sync::Arc;
    use std::sync::Mutex;

    // Records the targets a guest asks the host to dial.
    #[derive(Default)]
    struct RecordingOriginator {
        dialed: Arc<Mutex<Vec<String>>>,
    }
    #[async_trait]
    impl CallOriginator for RecordingOriginator {
        async fn place_call(&self, target: &str) -> Result<String, CallError> {
            self.dialed.lock().unwrap().push(target.to_owned());
            Ok("call-xyz".into())
        }
        async fn hangup(&self, _: &str) -> Result<(), CallError> {
            Ok(())
        }
    }

    // "sip:bob@host:5060" is 17 bytes at offset 0.
    let wat = r#"
(module
  (import "smiths" "originate" (func $orig (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "sip:bob@host:5060")
  (func (export "run")
    (call $orig (i32.const 0) (i32.const 17))
    drop))
"#;
    let originator = Arc::new(RecordingOriginator::default());
    let dialed = Arc::clone(&originator.dialed);

    let engine = WasmEngine::new().unwrap();
    engine
        .originator_slot()
        .set(originator as Arc<dyn CallOriginator>)
        .ok()
        .expect("set originator");
    engine.set_plugin_permissions("dialer", ["send_sip"]);
    let module = compile(&engine, wat);
    engine
        .run_entry(&module, "run", FUEL, "dialer")
        .expect("run_entry");

    // place_call is dispatched fire-and-forget — yield to let it land.
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        if !dialed.lock().unwrap().is_empty() {
            break;
        }
    }
    let recorded = dialed.lock().unwrap().clone();
    assert_eq!(recorded, vec!["sip:bob@host:5060".to_owned()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn originate_without_permission_traps() {
    let wat = r#"
(module
  (import "smiths" "originate" (func $orig (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "sip:x@y")
  (func (export "run")
    (call $orig (i32.const 0) (i32.const 7))
    drop))
"#;
    // No originator bound and no permission registered — the
    // permission check fires first, before the originator lookup.
    let engine = WasmEngine::new().unwrap();
    let module = compile(&engine, wat);
    let err = engine
        .run_entry(&module, "run", FUEL, "no-perm")
        .unwrap_err();
    assert!(
        matches!(err, WasmError::PermissionDenied { ref permission, .. } if permission == "send_sip"),
        "expected PermissionDenied(send_sip), got {err:?}"
    );
}

#[test]
fn call_invoke_error_envelope_becomes_plugin_error() {
    let wat = r#"
(module
  (memory (export "memory") 1)
  (data (i32.const 0) "{\"error\":\"nope\"}")
  (global $HEAP (mut i32) (i32.const 512))
  (func (export "alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $HEAP))
    (global.set $HEAP (i32.add (global.get $HEAP) (local.get $len)))
    (local.get $ptr))
  (func (export "invoke") (param i32 i32 i32 i32) (result i64)
    ;; (0 << 32) | 16
    i64.const 16))
"#;
    let engine = WasmEngine::new().unwrap();
    let module = compile(&engine, wat);
    let err = engine
        .call_invoke(&module, "err-demo", "anything", &serde_json::Value::Null)
        .unwrap_err();
    match err {
        WasmError::PluginError(msg) => assert_eq!(msg, "nope"),
        other => panic!("expected PluginError, got {other:?}"),
    }
}

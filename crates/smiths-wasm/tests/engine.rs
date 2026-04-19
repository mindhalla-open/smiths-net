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

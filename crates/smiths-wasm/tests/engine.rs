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

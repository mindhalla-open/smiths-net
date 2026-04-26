// Minimal Rhai plugin — hello-world cookbook recipe.
//
// Exports `describe_capabilities()` and `on_load()`.
// The engine calls `on_load()` once at plugin start and
// `describe_capabilities()` to discover what the plugin provides.

fn describe_capabilities() {
    [#{
        "capability": "greeting.hello",
        "plugin": "hello-world-rhai",
        "model_id": "hello-rhai-v1",
        "abi": "1.0",
        "description": "Minimal Rhai plugin — logs a greeting.",
        "priority": 50,
    }]
}

fn on_load() {
    print("hello-world-rhai: plugin loaded ✓");
}

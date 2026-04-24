# smiths-config-macros

Proc-macros for `smiths-net`'s config diff layer.

Today the crate ships one derive: `#[derive(Reloadable)]`.
Applied to any config struct, it generates a
`Reloadable::diff_into(&self, new, report, path_prefix)`
implementation that walks the struct's fields and pushes each
changed field's dotted path into `ApplyReport::reloaded` or
`ApplyReport::restart_required`.

The derive exists to keep the reloadable-field list from
drifting silently when a new config block lands. Instead of
editing a hand-maintained function in `reloader.rs` (and
remembering to), the developer classifies the field next to its
declaration.

## Attribute cheatsheet

```rust
use smiths_core::reloader::Reloadable;

#[derive(Reloadable)]
struct Section {
    // Live-reloadable. Path defaults to `<prefix>.field_name`.
    #[reloadable]
    log_level: String,

    // Live-reloadable with an explicit path (composite — any
    // change inside the nested struct collapses to one entry).
    #[reloadable(path = "section.rate_limit")]
    rate_limit: RateLimit,

    // Restart-required. Path defaults to `<prefix>.field_name`.
    #[restart_required]
    dir: PathBuf,

    // Restart-required, coalesced: all fields in this struct
    // sharing the same `group = "..."` label emit exactly one
    // report entry (with that label) when *any* of them change.
    #[restart_required(group = "section bind")]
    bind: SocketAddr,
    #[restart_required(group = "section bind")]
    enabled: bool,

    // Recurse into a field whose type also derives Reloadable.
    // The field's name is appended to `path_prefix` before the
    // nested walk runs.
    #[nested]
    sub: SubSection,

    // No attribute → the field is skipped silently. Use
    // sparingly: this is the "opt-out of the diff" knob, and
    // new unmarked fields will not show up in ApplyReport at
    // all. The point of the derive is to keep tracked fields
    // aligned with the struct; skip means "I've reviewed this
    // and it's deliberately out of scope."
    internal_state: Cache,
}
```

## Decision tree

When you add a new config field, pick exactly one:

```
┌─────────────────────────────────────────────────────────────┐
│ Does changing this field require restarting a bound socket, │
│ swapping a DB backend, or reloading plugins from disk?      │
│   Yes → #[restart_required]                                 │
│          (use #[restart_required(group = "label")] when     │
│           multiple fields in this struct collapse into one  │
│           operator-facing restart reason)                   │
└─────────────────────────────────────────────────────────────┘
                        │
                        ▼
┌─────────────────────────────────────────────────────────────┐
│ Can a running subsystem adopt the new value without         │
│ dropping live state (atomic swap, LRU resize, key rotation  │
│ on next fetch, etc.)?                                       │
│   Yes → #[reloadable]                                       │
│          (use #[reloadable(path = "x.y")] when composite:   │
│           any change inside a nested struct should collapse │
│           to one report entry at a custom dotted path)      │
└─────────────────────────────────────────────────────────────┘
                        │
                        ▼
┌─────────────────────────────────────────────────────────────┐
│ Is this field a nested config struct whose own fields carry │
│ their own reloadable/restart annotations?                   │
│   Yes → #[nested]                                           │
└─────────────────────────────────────────────────────────────┘
                        │
                        ▼
┌─────────────────────────────────────────────────────────────┐
│ Not relevant to hot-reload (internal bookkeeping, scaffold  │
│ fields not yet wired)?                                      │
│   Yes → no attribute                                        │
└─────────────────────────────────────────────────────────────┘
```

## Generated shape

Given:

```rust
#[derive(Reloadable)]
struct Section {
    #[reloadable]
    log_level: String,
    #[restart_required(group = "section bind")]
    bind: SocketAddr,
    #[restart_required(group = "section bind")]
    enabled: bool,
}
```

The derive expands roughly to:

```rust
impl crate::reloader::Reloadable for Section {
    fn diff_into(
        &self,
        new: &Self,
        report: &mut crate::reloader::ApplyReport,
        path_prefix: &str,
    ) {
        if self.log_level != new.log_level {
            report.reloaded.push(
                if path_prefix.is_empty() {
                    String::from("log_level")
                } else {
                    format!("{}.log_level", path_prefix)
                },
            );
        }
        if self.bind != new.bind || self.enabled != new.enabled {
            report.restart_required.push(String::from("section bind"));
        }
    }
}
```

## Current scope

- `#[reloadable]` on composite fields requires the field's type
  to implement `PartialEq`. `Reloadable` is only required for
  `#[nested]` fields.
- The derive generates absolute paths `crate::reloader::…` and
  is therefore intended for `smiths-core` use. If a future
  caller needs it in another crate, flip the generated paths to
  `::smiths_core::reloader::…` in `lib.rs` — nothing else changes.
- Compile-time enforcement of "every field must be classified"
  is not shipped: unmarked fields are silently skipped. This is
  deliberate — the existing config has many fields that aren't
  part of the reload surface today. A future slice can add a
  `#[derive(Reloadable)]` struct-level `strict` flag that errors
  on unmarked fields for operators who want that safety.

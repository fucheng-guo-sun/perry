# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

**NOTE**: Keep this file concise. Detailed changelogs live in CHANGELOG.md.

## Project Overview

Perry is a native TypeScript compiler written in Rust that compiles TypeScript source code directly to native executables. It uses SWC for TypeScript parsing and LLVM for code generation.

**Current Version:** 0.5.505


## TypeScript Parity Status

Tracked via the gap test suite (`test-files/test_gap_*.ts`, 28 tests). Compared byte-for-byte against `node --experimental-strip-types`. Run via `/tmp/run_gap_tests.sh` after `cargo build --release -p perry-runtime -p perry-stdlib -p perry`.

**Last full sweep:** run `./run_parity_tests.sh` for the current snapshot — the gap-suite top-line has shifted with recent landings; see open issues #447 (nested-async hang masking `test_async`), #448 (`*[Symbol.iterator]()` class hang), #449 (`new.target` → NaN), #450 (`defineProperty` accessor `this`), #451 (`test_gap_json_advanced` SIGSEGV), and #452 (`Array.fromAsync` / Proxy/Reflect / JSON ordering). Several of these are pre-existing bugs that the parity skip-list and gap-suite output truncation hid from earlier numbers.

**Known categorical gaps**: lookbehind regex (Rust `regex` crate), `console.dir`/`console.group*` formatting, lone surrogate handling (WTF-8).

## Workflow Requirements

**IMPORTANT:** Follow these practices for every code change made directly on `main` (maintainer workflow):

1. **Update CLAUDE.md**: Add 1-2 line entry in "Recent Changes" for new features/fixes
2. **Increment Version**: Bump patch version (e.g., 0.5.48 → 0.5.49)
3. **Commit Changes**: Include code changes and CLAUDE.md updates together

### External contributor PRs

PRs from outside contributors should **not** touch `[workspace.package] version` in `Cargo.toml`, the `**Current Version:**` line in `CLAUDE.md`, or add a "Recent Changes" entry. The maintainer bumps the version and writes the changelog entry at merge time — usually by rebasing the PR branch and amending. This avoids the patch-version collisions that happen when Perry's `main` ships several commits while a PR is in review (each on-main commit bumps the version; a PR that bumped to the same patch on day 1 is already behind by merge day). Contributors just write code; let the maintainer fold in the metadata last.

## Build Commands

```bash
cargo build --release                          # Build all crates
cargo build --release -p perry-runtime -p perry-stdlib  # Rebuild runtime (MUST rebuild stdlib too!)
cargo test --release --workspace \
  --exclude perry-ui-ios --exclude perry-ui-tvos --exclude perry-ui-watchos \
  --exclude perry-ui-visionos --exclude perry-ui-android --exclude perry-ui-windows \
  --exclude perry-ui-gtk4   # Run tests (exclude cross-host UI crates on macOS)
cargo run --release -- file.ts -o output && ./output    # Compile and run TypeScript
cargo run --release -- file.ts --print-hir              # Debug: print HIR
```

## Architecture

```
TypeScript (.ts) → Parse (SWC) → AST → Lower → HIR → Transform → Codegen (LLVM) → .o → Link (cc) → Executable
```

| Crate | Purpose |
|-------|---------|
| **perry** | CLI driver (parallel module codegen via rayon) |
| **perry-parser** | SWC wrapper for TypeScript parsing |
| **perry-types** | Type system definitions |
| **perry-hir** | HIR data structures (`ir.rs`) and AST→HIR lowering (`lower.rs`) |
| **perry-transform** | IR passes (closure conversion, async lowering, inlining) |
| **perry-codegen** | LLVM-based native code generation |
| **perry-runtime** | Runtime: value.rs, object.rs, array.rs, string.rs, gc.rs, arena.rs, thread.rs |
| **perry-stdlib** | Node.js API support (mysql2, redis, fetch, fastify, ws, etc.) |
| **perry-ui** / **perry-ui-macos** / **perry-ui-ios** / **perry-ui-tvos** | Native UI (AppKit/UIKit) |
| **perry-jsruntime** | JavaScript interop via QuickJS |

## NaN-Boxing

Perry uses NaN-boxing to represent JavaScript values in 64 bits (`perry-runtime/src/value.rs`):

```
TAG_UNDEFINED = 0x7FFC_0000_0000_0001    BIGINT_TAG  = 0x7FFA (lower 48 = ptr)
TAG_NULL      = 0x7FFC_0000_0000_0002    POINTER_TAG = 0x7FFD (lower 48 = ptr)
TAG_FALSE     = 0x7FFC_0000_0000_0003    INT32_TAG   = 0x7FFE (lower 32 = int)
TAG_TRUE      = 0x7FFC_0000_0000_0004    STRING_TAG  = 0x7FFF (lower 48 = ptr)
```

Key functions: `js_nanbox_string/pointer/bigint`, `js_nanbox_get_pointer`, `js_get_string_pointer_unified`, `js_jsvalue_to_string`, `js_is_truthy`

**Module-level variables**: Strings stored as F64 (NaN-boxed), Arrays/Objects as I64 (raw pointers). Access via `module_var_data_ids`.

## Garbage Collection

Generational mark-sweep GC in `crates/perry-runtime/src/gc.rs` (default since v0.5.237 / Phase D). Two regions in the per-thread arena: nursery (`ARENA`, fills with new allocations, swept on minor GC) and old-gen (`OLD_ARENA`, holds tenured/evacuated objects). Conservative stack scan + precise shadow-stack roots + 9 registered scanners. Write barriers populate a remembered set so minor GC can avoid retracing the old-gen. Two-bit aging (`HAS_SURVIVED` / `TENURED`) promotes nursery survivors after 2 minor cycles; the C4b evacuation pass moves non-pinned tenured objects into old-gen with full reference rewriting. Idle nursery blocks observed empty for 2 GC cycles are `dealloc`'d back to the OS (C4b-δ, v0.5.235), and the next-trigger calc is hard-capped at the initial threshold (64 MB) so >90%-freed step-doubling can't blow up peak occupancy (C4b-δ-tune, v0.5.236). Triggers on arena block allocation (1 MB blocks since v0.5.196), malloc count threshold, or explicit `gc()` call. 8-byte GcHeader per allocation.

**Escape hatches**: `PERRY_GEN_GC=0`/`off`/`false` reverts to full mark-sweep (bisection only). `PERRY_GEN_GC_EVACUATE=1` enables the copying evacuation pass (default OFF — complete and correctness-safe but adds work that's a no-op on workloads where nothing tenures). `PERRY_WRITE_BARRIERS=1` opts into codegen-emitted write barriers (default OFF — barrier emission has its own perf cost; the runtime barrier always exists). `PERRY_GC_DIAG=1` prints per-cycle diagnostics.

## Threading (`perry/thread`)

Single-threaded by default. `perry/thread` provides:
- **`parallelMap(array, fn)`** / **`parallelFilter(array, fn)`** — data-parallel across all cores
- **`spawn(fn)`** — background OS thread, returns Promise

Values cross threads via `SerializedValue` deep-copy. Each thread has independent arena + GC. Results from `spawn` flow back via `PENDING_THREAD_RESULTS` queue, drained during `js_promise_run_microtasks()`.

## Native UI (`perry/ui`)

Declarative TypeScript compiles to AppKit/UIKit calls. Handle-based widget system (1-based i64 handles, NaN-boxed with POINTER_TAG). `--target ios-simulator`/`--target ios`/`--target tvos-simulator`/`--target tvos` for cross-compilation.

**To add a new widget** — change 4 places:
1. Runtime: `crates/perry-ui-macos/src/widgets/` — create widget, `register_widget(view)`
2. FFI: `crates/perry-ui-macos/src/lib.rs` — `#[no_mangle] pub extern "C" fn perry_ui_<widget>_create`
3. Codegen: `crates/perry-codegen/src/codegen.rs` — declare extern + NativeMethodCall dispatch
4. HIR: `crates/perry-hir/src/lower.rs` — only if widget has instance methods

## Compiling npm Packages Natively (`perry.compilePackages`)

Configured in `package.json`:
```json
{ "perry": { "compilePackages": ["@noble/curves", "@noble/hashes"] } }
```
First-resolved directory cached in `compile_package_dirs`; subsequent imports redirect to the same copy (dedup).

## Known Limitations

- **No runtime type checking**: Types erased at compile time. `typeof` via NaN-boxing tags. `instanceof` via class ID chain.
- **No shared mutable state across threads**: No `SharedArrayBuffer` or `Atomics`.

## Common Pitfalls & Patterns

### NaN-Boxing Mistakes
- **Double NaN-boxing**: If value is already F64, don't NaN-box again. Check `builder.func.dfg.value_type(val)`.
- **Wrong tag**: Strings=STRING_TAG, objects=POINTER_TAG, BigInt=BIGINT_TAG.
- **`as f64` vs `from_bits`**: `u64 as f64` is numeric conversion (WRONG). Use `f64::from_bits(u64)` to preserve bits.

### LLVM Type Mismatches
- Loop counter optimization produces i32 — always convert before passing to f64/i64 functions
- Constructor parameters always f64 (NaN-boxed) at signature level

### Async / Threading
- Thread-local arenas: JSValues from tokio workers invalid on main thread
- Use `spawn_for_promise_deferred()` — return raw Rust data, convert to JSValue on main thread
- Async closures: Promise pointer (I64) must be NaN-boxed with POINTER_TAG before returning as F64

### Cross-Module Issues
- ExternFuncRef values are NaN-boxed — use `js_nanbox_get_pointer` to extract
- Module init order: topological sort by import dependencies
- Optional params need `imported_func_param_counts` propagation through re-exports

### Closure Captures
- `collect_local_refs_expr()` must handle all expression types — catch-all silently skips refs
- Captured string/pointer values must be NaN-boxed before storing, not raw bitcast
- Loop counter i32 values: `fcvt_from_sint` to f64 before capture storage

### Handle-Based Dispatch
- TWO systems: `HANDLE_METHOD_DISPATCH` (methods) and `HANDLE_PROPERTY_DISPATCH` (properties)
- Both must be registered. Small pointer detection: value < 0x100000 = handle.

### objc2 v0.6 API
- `define_class!` with `#[unsafe(super(NSObject))]`, `msg_send!` returns `Retained` directly
- All AppKit constructors require `MainThreadMarker`

## Recent Changes

Keep entries to 1-2 lines max. Full details in CHANGELOG.md.

- **v0.5.505** — Closes #449: fold `new.target.<prop>` and `new.target?.<prop>` to a string/undefined literal at HIR lowering, bypassing the broken `MetaProp(NewTarget)` Object-literal path that returned `NaN` for `.name`.
- **v0.5.504** — Closes #453: refresh stale CLAUDE.md gap-sweep claim, broaden workspace-test exclude list to all cross-host UI crates, regenerate `honest_bench/REPORT.md` against current `results.json` (perry 0.5.495 numbers).
- **v0.5.503** — Closes #446: bind class-method PropertyGet via `js_class_method_bind` + flow type-only-import class metadata into `imported_classes` so dispatch tables see the methods.
- **v0.5.502** — Closes #444: fold `import.meta.{url,main,dirname,filename}` to literal at lowering, bypassing the broken module-globals object path.
- **v0.5.501** — Closes #422: `new net.Socket()` constructor + deferred `sock.connect(port, host)` instance method + `net.connect(...)` factory alias — pure-TS TCP clients now work end-to-end.
- **v0.5.500** — Closes #429: GTK4 `appSetTimer` callbacks scheduled mid-event-loop now install immediately instead of being queued forever after `connect_activate`.
- **v0.5.499** — Closes #431: skip `imported_class_prefix` for class names that collide with a local class — local methods now emit under the correct module prefix.
- **v0.5.498** — Closes #443: NaN-boxed-string check in `format_value` for windows/android/gtk4 state binding so string-typed `State` no longer renders as literal `"NaN"`.
- **v0.5.497** — Closes #442: flip `apply_inline_style` FFI declarations from `DOUBLE` to `VOID` so inline `Button(label, onPress, { backgroundColor })` styling matches the explicit setter form.
- **v0.5.496** — Closes #435: take a transitive closure backward through writes when collecting `index_used_locals`, so pure accumulators stay off the i32 shadow path that silently truncated 64-bit sums.
- **v0.5.495** — `perry-transform` static-trip-count for-loop full-unroll pass — closes 80-90 ms of `image_convolution`'s gap to Zig (22% improvement).
- **v0.5.494** — perry-hir build fix (drop stale `lower_module_with_class_id_types_and_seed` re-export) + closes #423 by adding gstreamer-1.0 link flags to the Linux GTK4 link branch.
- **v0.5.493** — codegen-arkts: `mutator_background_color` resolves through bindings + new `buttonSetTextColor` / `buttonSetBordered` handlers — Mango's HarmonyOS welcome screen now matches the macOS reference.
- **v0.5.492** — codegen-arkts: fix `textSetFontWeight` arg semantics, chase `LocalGet` aliases up to 16 hops, recognize HarmonyOS-stubbed functions as zero literals.
- **v0.5.491** — codegen-arkts: dead-branch elim for unfoldable conditions + i18n `t()` unwrapping + recursive expression-level inlining + early-return → if/else rewrite.
- **v0.5.490** — Closes #414: thread `params: JSValue` through mysql2's `*_query` runtime functions so `db.query(sql, [param])` no longer drops bindings (and stops bricking the connection with error 1835).
- **v0.5.489** — codegen-arkts: inline top-level user-function calls into a per-harvest analysis copy of `module.init` so procedural-mutation collectors see widget mutations inside function bodies.
- **v0.5.488** — codegen-arkts: ternary-resolving image asset paths + numeric ternary resolution (`mobile ? 40 : 44`) + HAP `resources/rawfile/` bundling for `assets/` paths.
- **v0.5.487** — codegen-arkts: `Expr::Conditional` arm in `emit_widget` resolves `mobile ? HStack(...) : HStack(...)` to one branch instead of falling through to `[unrecognized body]`.
- **v0.5.486** — codegen-arkts: `collect_const_bindings` recurses into `Stmt::If` branches so `const widget = Button(...)` inside `if (mobile)` is harvestable.
- **v0.5.485** — #408/#410/#413 polish: text styling mutators, binding-aware numeric resolver, real `widgetSetBackgroundGradient`, comment-eats-modifier fix.
- **v0.5.484** — #413 follow-up: `stackSetAlignment` value-name table is axis-aware (`VerticalAlign.Top` not `.Start` for HStack).
- **v0.5.483** — Closes #413: `evaluate_condition` constant-folder + `stack_axis_align_enum` (Row→VerticalAlign, Column→HorizontalAlign) + `wrap`/`needs_parens` for operator-precedence-correct serialization.
- **v0.5.482** — Windows source-build hotfix: relocate `[target.'cfg(unix)'.dependencies]` block from inside `[dependencies]` to file end (it had silently scoped tokio/hyper/sqlx/etc to Unix-only).
- **v0.5.481** — Closes #410: `serialize_condition` returns clean `"true"` instead of `*/`-leaking diagnostic, resolves `LocalGet` chains, inlines `__platform__` as a numeric literal.
- **v0.5.480** — Closes #408: `crates/perry-codegen-arkts` follows post-construction widget mutations (`widgetAddChild` etc.) on `--target harmonyos` instead of leaving containers empty.
- **v0.5.479** — Two HarmonyOS link follow-ups: register `vstack/hstack/button_create` direct stubs + add `-ltime_service_ndk` for `iana-time-zone`'s OHOS Rust target.
- **v0.5.478** — perry/tui Phase 4.5: `Spinner` / `Input` / `List` / `Select` / `TextArea` widgets close out the v1 spec from #358; widget-factory FFIs now consistently return raw `i64` handles.
- **v0.5.477** — Closes #395 + #399 + #400: auto-generated no-op stubs for every `perry_ui_*` / `perry_system_*` / `perry_updater_*` symbol in `perry-runtime`'s build.rs so harmonyos `.so` loads.
- **v0.5.476** — Closes #392 (followup): widen `skip_native` so unknown class names route through `js_native_call_method` + `CLASS_VTABLE_REGISTRY` for type-only-imported class params.
- **v0.5.475** — Closes #358 (Phase 4): `Spacer` + `ProgressBar` widgets, `flexGrow` BoxStyle, headline acceptance criterion #2 no-flicker proof.
- **v0.5.474** — #358 Phase 3: Taffy flexbox layout for perry/tui — `Box({ flexDirection, justifyContent, alignItems, gap, padding, width, height }, [children])` routes through Taffy 0.7.

Older entries → CHANGELOG.md.

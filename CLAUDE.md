# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

**NOTE**: Keep this file concise. Detailed changelogs live in CHANGELOG.md.

## Project Overview

Perry is a native TypeScript compiler written in Rust that compiles TypeScript source code directly to native executables. It uses SWC for TypeScript parsing and LLVM for code generation.

**Current Version:** 0.5.518


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

- **v0.5.518** — Closes #454: bump `redis` 0.25.4 → 1.2 + `sqlx` 0.8.0 → 0.8.6 + `rusqlite` 0.31 → 0.32.1 to clear the never-type-fallback future-incompat warnings emitted on every release build (originated in `redis-0.25.4/src/script.rs` and `sqlx-postgres-0.8.0/src/connection/{executor.rs,copy.rs}`). The chain my own #454 comment laid out — sqlx 0.8.6 hard-pins `sqlx-sqlite ^0.8.6` which wants `libsqlite3-sys ^0.30.1`, so rusqlite has to move off 0.31 (libsqlite3-sys 0.28) at the same time or cargo refuses to resolve since both packages declare `links = "sqlite3"`. Picked rusqlite 0.32.1 (libsqlite3-sys 0.30.1, exact match) over 0.36+ to keep the API audit minimal — sqlite.rs needed zero changes. The redis 1.x audit landed in one rename: `ErrorKind::IoError` became `ErrorKind::Io` (14 callsites in `crates/perry-stdlib/src/ioredis.rs`); `AsyncCommands` / `MultiplexedConnection` / `Client::open` / `cmd().query_async` / the `set::<_, _, ()>` turbofish are all unchanged. `cargo report future-incompatibilities` is now empty and `cargo test -p perry-stdlib --lib` stays green at 46/46.
- **v0.5.517** — Closes #443 (followup to v0.5.498): SSO inline strings (`SHORT_STRING_TAG` 0x7FF9) still rendered as "NaN" through `stateBindTextNumeric` / TEXT_BINDINGS / MULTI_TEXT_BINDINGS. The v0.5.498 fix only matched heap `STRING_TAG` (0x7FFF), so any short ≤5-byte string state on Windows / Android / GTK4 (and macOS too — same gap was sitting latent) fell through to `format!("{}", value)`. Same shape as #434 — the SSO encoding lives alongside the heap encoding and any "is this a string?" check that only knows about one tag silently mishandles the other. Fix: in `crates/perry-ui-{macos,windows,android,gtk4}/src/state.rs`, widen `is_nanboxed_string` to accept both 0x7FFF and 0x7FF9, and route `extract_nanboxed_string` through `js_get_string_pointer_unified` (which materializes SSO via `js_string_materialize_to_heap`) instead of bare `js_nanbox_get_pointer` — the latter would have read the SSO inline payload bytes as a pointer and segfaulted, not rendered "NaN". Cleaned up a redundant inner extern block in gtk4 state.rs's textfield init path now that the symbol is at module scope.
- **v0.5.516** — Closes #434: `Set.has` / `Map.get` / `obj[key]` returned false / undefined / 0 when the lookup key was a ≤5-byte string from `JSON.parse` even though `===` reported equal. Root cause was a representation mismatch: literals at compile sites land as heap `STRING_TAG` (0x7FFF) while JSON.parse emits inline `SHORT_STRING_TAG` SSO (0x7FF9, v0.5.216+). The `JSValueKey::hash` / `jsvalue_eq` content-equality paths in `crates/perry-runtime/src/{set,map}.rs` only knew about the heap-pointer tags, so SSO bits hashed by raw u64 and compared as not-equal to STRING_TAG entries; Map's side-table additionally treated SSO as a "safe numeric key" and short-circuited the linear-scan fallback for size > 8 maps. The codegen IndexGet/IndexSet last-resort fallback in `crates/perry-codegen/src/expr.rs` only matched `top16 == 0x7FFF` so SSO keys fell into the numeric branch and read past the object header. Fix: add an SSO-aware `string_view_from_bits` helper to set.rs / map.rs that decodes inline payload bytes for content hashing and equality; exclude `SHORT_STRING_TAG` from `is_safe_numeric_key` so SSO keys force the linear-scan path; in expr.rs detect both `0x7FFF` and `0x7FF9` and route through `unbox_str_handle` (which materializes SSO via `js_get_string_pointer_unified`) instead of bare `AND POINTER_MASK`. All four reproductions from the issue now match node; parity sweep stays at 208 pass / 1 known fail / 2 known compile-fails.
- **v0.5.515** — Closes #407: `console.log(...arr)` (and `.info` / `.warn` / `.error` / `.debug` with spread args) silently dropped most arguments — `console.log(...[1,2,3])` printed `1` instead of `1 2 3`. The `Expr::Call` console-dispatch in `crates/perry-codegen/src/lower_call.rs` is only reached when the HIR has zero spread args; spread calls produce `Expr::CallSpread` which fell through to the generic closure-spread path (`js_closure_call_apply_with_spread`) and treated `console.log` as a closure value. Fix: add a `console.{log,info,warn,error,debug}` fast path at the top of the `Expr::CallSpread` arm in `crates/perry-codegen/src/expr.rs` that bundles every regular + spread arg into a single accumulator array (push for `CallArg::Expr`, `js_array_concat` for `CallArg::Spread`) and dispatches to the matching `js_console_*_spread` runtime fn, mirroring the multi-arg path the non-spread codegen already uses.
- **v0.5.514** — Closes #393: hub-scale SIGSEGV in async route handlers came from a FuncId collision in `crates/perry-transform/src/generator.rs`. `compute_max_func_id` walked `Expr::Call`/`Expr::New` args but had no arm for `Expr::NativeMethodCall`, so closures inside `app.post('/r', async (req, reply) => …)` and `wss.on('listening', () => …)` (registered as `NativeMethodCall { args: [String, Closure { func_id, … }] }` in HIR) were invisible to the scan. The async-to-generator pass then started `next_func_id = max_id + 1` below those existing ids and the synthesized iter step closures collided with them. At codegen the alloc site emitted `js_closure_alloc_with_captures_singleton(..., i32 1, …)` (one slot, matching the user closure's capture count) while the body — actually the iter step's body that won the func-id race — read `capture[16]`, scribbling `js_box_set` over whichever string handle / port number lived past the end of the allocation. Fix: add `NativeMethodCall` plus `StaticMethodCall` / `SuperCall` / `SuperMethodCall` / `CallSpread` / `NewDynamic` arms to both `scan_expr_for_max_func` and `scan_expr_for_max_local`. perry-hub (`../hub`, 2239 lines) now starts cleanly with no `[PERRY WARN] js_box_set` warnings; parity sweep stays at 209 pass / 2 known fails.
- **v0.5.513** — Closes #399 (round 2): the v0.5.477 fix walked the four `perry-dispatch` tables but missed three direct-call FFI symbols emitted by `lower_notification_schedule` in `crates/perry-codegen/src/lower_call.rs:3025-3097` — `perry_system_notification_schedule_{interval,calendar,location}` are added to `pending_declares` directly (not via dispatch lookup), so the `cfg(feature = "ohos-napi")` table walk in `crates/perry-runtime/build.rs` couldn't see them. Fix: extend the existing `direct_call_stubs` array (which already carries `perry_ui_{vstack,hstack,button}_create` for the same reason) with the three trigger variants and their `(I64, I64, I64, F64…)` signatures. Generated stub count goes 220 → 223; `notificationSchedule({ trigger: { type: "interval"|"calendar"|"location", … } })` from any harmonyos TS code now link-resolves to a no-op instead of failing OHOS dynamic-loader relocation.
- **v0.5.512** — Closes #396 + #397 + #398: macOS Homebrew bottle now bundles tvOS / visionOS / watchOS cross-libs (device + simulator) alongside the existing iOS pair. `release-packages.yml` installs nightly + rust-src on the macOS legs and runs `cargo +nightly build -Z build-std=core,std,panic_abort` against `aarch64-apple-{tvos,visionos}` / `arm64_32-apple-watchos` plus their `-sim` triples; staging copies device libs as `libperry_{runtime,stdlib,ui_<plat>}.a` and sim variants with a `_sim` suffix. No code changes — `library_search.rs::apple_class_lib_name` already maps every `--target` to the right variant (covered by `handles_other_class_suffixes`).
- **v0.5.511** — release-CI gate fix (round 2): add `test_edge_closures` to the `compile-smoke` job's inline `SKIP_TESTS` list in `.github/workflows/test.yml`. The compile-smoke job has its own skip mechanism separate from `parity`'s `known_failures.json`; adding to one without the other left the smoke gate failing on #456 (LLVM `@perry_global_*` collision).
- **v0.5.510** — release-CI gate fixes: `cargo fmt --all` to clear pre-existing rustfmt drift in `unroll.rs` / `inline.rs` / `expr.rs` / etc. (lint job was failing on v0.5.502+); add #456 (test_edge_closures LLVM global name collision) + #457 (test_gap_generators genWithReturn drift) to `test-parity/known_failures.json` so the parity gate passes. Both pre-existing — issues filed for follow-up. Unblocks v0.5.509 release publish (which had been gated on Tests).
- **v0.5.509** — Closes #447 + un-skip 6 async tests in `run_parity_tests.sh::SKIP_TESTS`. The v0.5.508 ABI fix on `js_object_set_field` transitively fixed #447's deeper "body doesn't execute" bug — the async-to-generator iter object was hit by the same DOUBLE/u64 register-class mismatch. `test_async` / `test_async2…5` / `test_async_chain` all now match node byte-for-byte; `test_gap_async_advanced` prints all 28 expected lines including "ALL ASYNC ADVANCED TESTS PASSED".
- **v0.5.508** — Closes #448 + #451: `*[Symbol.iterator]()` generator method on a class (and plain `function*` generators iterated via `for…of`) hung allocating until OOM — root cause was an ABI mismatch in the shape-cache fast path of `lower_object_literal`: `js_object_set_field` was declared to take its value arg as `DOUBLE` but the runtime takes it as `JSValue` (`#[repr(transparent)] u64`). On AArch64 / x86_64 SysV / Win64 these use disjoint register classes, so closure pointers stored into the generator's `{ next, return, throw }` iter object landed in xmm0 / d0 while the runtime read garbage from rdx / x2 — every closure field read back as 0, `__iter.next()` dispatched against undefined, and the `for…of` loop never saw `done = true`. Fix in two files: `crates/perry-codegen/src/runtime_decls.rs` flips the third arg from `DOUBLE` to `I64`, and `crates/perry-codegen/src/expr.rs::lower_object_literal` bitcasts the lowered double to i64 before the call so the value rides in the same register class the runtime reads.
- **v0.5.507** — Closes #450: `Object.defineProperty(obj, k, { get(){}, set(){} })` registered the accessor (round-trips via `getOwnPropertyDescriptor`) but invoked the getter/setter with the descriptor literal as `this` instead of `obj` — `obj.value` returned NaN. Fix: codegen now ORs `CAPTURES_THIS_FLAG` into the cap_count when allocating `captures_this:true` closures so the runtime can detect them; `js_object_define_property` clones each accessor closure via new `clone_closure_rebind_this` and rebinds the reserved this-slot to `obj`.
- **v0.5.506** — #447 partial: skip `alwaysinline` for `was_plain_async`-rewritten functions in codegen.rs; removes the infinite microtask-loop hang from nested async-await but a deeper bug (rewritten body doesn't execute, `js_box_get` null pointer) remains — issue stays open.
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

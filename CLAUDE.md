# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

**NOTE**: Keep this file concise. Detailed changelogs live in CHANGELOG.md.

## Project Overview

Perry is a native TypeScript compiler written in Rust that compiles TypeScript source code directly to native executables. It uses SWC for TypeScript parsing and LLVM for code generation.

**Current Version:** 0.5.612


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

One-liners only — full detail in CHANGELOG.md.

- **v0.5.612** — Refs #421 (two more hono internals fixes): (1) v0.5.607's any-typed string-method dispatch tower returned `JSValue::int32(i).bits()` (NaN-boxed INT32_TAG) for `indexOf`/`lastIndexOf`, but INT32_TAG values are NaN — comparisons like `idx < url.length` always returned false because NaN comparisons are false. Hono's `getPath()` does `for (; i < url.length; i++)` after `i = url.indexOf("/", colonIdx + 4)` — the loop never iterated, getPath returned empty string, the route never matched, dispatch fell through to compose, "Context is not finalized" fired. Fix: return `i as f64` directly (matches typed-path's `sitofp` from `lower_string_method.rs`). Same pattern for `lastIndexOf`. Also added `replace`/`replaceAll` arms to the same dispatch tower (string + RegExp pattern; function-replacement deferred). (2) Closures called with fewer args than declared got garbage in missing slots — calling `c.text('hello')` (1 arg) on a `(text, arg, headers) => …` arrow stored as a class field went through `js_native_call_method` field-scan → `js_native_call_value(..., args_len=1)` which transmuted func_ptr to a 1-arg signature; the closure body's args 2 and 3 read from random stack registers. Hono's `c.text`'s `headers` slot evaluated to a tiny denormal float, fast-path condition `!headers` was false, slow-path `setDefaultContentType(TEXT_PLAIN, headers)` ran and the resulting header object was processed through `responseHeaders.set(k, v)` which then hit the issue #510 catch-all `(number).set is not a function`. Fix: new `CLOSURE_ARITY_REGISTRY` thread-local in `crates/perry-runtime/src/closure.rs` mapping non-rest closures' func_ptr → declared arity, populated at module init via new `js_register_closure_arity` extern (parallel to the existing closure-rest registry). `js_native_call_value` consults the registry; when the closure's declared arity exceeds `args_len`, pads trailing slots with TAG_UNDEFINED and dispatches to the matching higher-arity `js_closure_callN` (1-8). Codegen `crates/perry-codegen/src/codegen.rs` builds a `closure_arities: HashMap<u32, u32>` alongside `closure_rest_params` and `emit_string_pool` registers each entry. End-to-end: hono's `app.fetch(req)` runs through `c.text` → `new Response('hello')` correctly returning a Response (was hitting "Context is not finalized" → `(number).set` chain pre-fix). Remaining hono blocker is a SIGSEGV deeper in the fetch dispatch chain — separate, distinct from the arity / int-tag / handle-NaN-boxing fixes. Parity: 209 pass / 4 known fails / 13 skipped (98.1%, no regressions vs main).
- **v0.5.611** — Closes #531: `ops.push(await runOp('name', N, async () => ...))` silently elided the inner closure's body — `compute_max_func_id` / `compute_max_local_id` in `crates/perry-transform/src/generator.rs` had no `Expr::ArrayPush` / `Expr::ArrayPushSpread` arms, so the user closure buried inside `ArrayPush.value: Await(Call { args: [..., Closure { func_id: K, ... }] })` was invisible to both scanners. The generator transform's `next_func_id` started below K, and the synthesized `__async_iter` `next`/`return`/`throw` closures got func_ids that collided with K. Codegen emits one LLVM function per func_id, so one definition wins and the other closure invokes it through a mismatched-shape capture array — calling `js_box_set` with a NULL or stale pointer (the bench's `LocalSet(__gen_done, true)` writes to whatever slot ended up in the capture position). Symptom in the @perryts/mongodb bench: `findOne_by_id_movie` and `find_movies_*` op closures' bodies never executed, with 3× `[PERRY WARN] js_box_set/get: invalid box pointer 0x0` warnings during their warmup loops. Fix: add `Expr::ArrayPush { value, .. }` and `Expr::ArrayPushSpread { source, .. }` arms to both scanners (mirrors the existing #154 / #393 / #212 arms for ArrayForEach/NativeMethodCall/etc.). Verified: bench's 9 ops now all execute correctly with 0 warnings, fingerprints match (find_movies = count 50, was count 1). Parity: 209 pass / 4 known fails / 13 skipped (98.1%, no regressions vs main).
- **v0.5.610** — Refs #421 (followup to v0.5.608): `recv.method(...args)` on any-typed receivers silently no-op'd — `Expr::CallSpread`'s closure-callee fallback evaluated `recv.method` as a property read which returned `undefined` for class-prototype methods, and `js_closure_call_apply_with_spread` then no-op'd. Hono's SmartRouter.match's inner `router.add(...routes[i])` hit this — inner routers never received the route entries, so match returned empty `[[],[]]` even when routes were registered. Fix: new `recv.method(...args)` arm in `Expr::CallSpread` (`crates/perry-codegen/src/expr.rs`) detects PropertyGet callees and routes through the new `js_native_call_method_apply` runtime helper which materialises a JS args array into a temp f64 buffer and forwards to `js_native_call_method`. Plus `replace`/`replaceAll` added to the any-typed string method dispatch tower (`crates/perry-runtime/src/object.rs`) — pattern can be string or RegExp; replacement must be string. Detect RegExp pattern via NaN-box POINTER_TAG check. Function replacements deferred (need closure dispatch). Hono's `await app.fetch(req)` now runs through the dispatch chain past route matching; remaining errors are deeper closure-box / context-finalization issues in hono internals (separate bug classes — pre-existing). Parity: 209 pass / 4 known fails / 13 skipped (98.1%, no regressions vs main).
- **v0.5.609** — Closes #528 (followup to #515/#518): chained-call array-method fold leaked through the `recv_is_class` `_ => false` catch-all — `this.col().find({})` got rewritten as `Expr::ArrayFind(this.col(), {})` and `js_array_find` read garbage from the user object's header. New `Call(inner)` arm in `recv_is_class` only allows the fold when the inner call's method is a known array-producing builtin; otherwise bails to runtime dispatch.
- **v0.5.608** — Refs #421: Phase 1 of handle-NaN-boxing unification — Web Fetch (Request/Response/Headers/Blob) handles now NaN-boxed at the FFI boundary; 25 fetch FFI fns migrated, 4 untyped property dispatch helpers added. Unblocks hono `request.url` in untyped position.
- **v0.5.607** — Closes #529 (followup to #515): `obj["method"](args)` / `obj["prop"]` on a class instance returned `undefined` — fold computed `MemberProp` with non-numeric string key to `Expr::PropertyGet` so dispatch hits the vtable; mirror fold in assignment form.
- **v0.5.606** — Closes #526: escape JS reserved words used as method names in generated `.d.ts` (axios.delete now emits `function _delete; export { _delete as delete }`).
- **v0.5.605** — Closes #515 (round 2): tighten `with`-method fold gates for object-literal / chained-method-call / typed-array receivers; add layered codegen + runtime fallbacks.
- **v0.5.604** — Closes #525: unify R005/#463 unimplemented-API message format for the 2-deep native-module call form (`m.method()`).
- **v0.5.603** — Refs #519/#421: register Request/Response/Headers typed-parameter annotations as native instances at HIR lowering so codegen's static dispatch fires.
- **v0.5.602** — Closes #509: parallel-codegen race producing colliding `.o` files — temp path nonce now uses an atomic counter alongside pid+nanos.
- **v0.5.601** — Closes #519: bind `this` for `instance.fn(args)` style cross-instance method calls via thread-local `IMPLICIT_THIS`; fill string-receiver `match`/`matchAll`/`search` arms.
- **v0.5.600** — Closes #461: pre-register `const f = (...) => …` arrow declarations as locals before lowering their initializer, so nested closures referencing `f` find it via `lookup_local`.
- **v0.5.600** — Closes #513: API manifest now enumerates every `NATIVE_MODULES` entry + every well-known binding (~250 entries backfilled) so the unimplemented-API gate flips strict for every supported module.
- **v0.5.599** — Closes #518: async class method's `await m.toArray()` returned `[]` because scalar-replacement of non-escaping object literals dropped the receiver-as-`this` for `js_native_call_method`.
- **v0.5.598** — Closes #512: API manifest now carries real param/return type signatures (`bcrypt.hash(password: string, …)`) instead of `(...args: any[]): any` fallback.
- **v0.5.597** — Closes #511 (followup to #462): `x.foo()` on undefined/null now throws `TypeError` and exits 1 (was silent no-op exit 0).
- **v0.5.596** — Refs #519: vtable `this` now NaN-boxed with POINTER_TAG before f64 cast so dispatched method bodies see a real instance pointer.
- **v0.5.595** — Closes #515: a class method named `with` with ≥2 params was getting folded into `Expr::ArrayWith` — gate the fold catch-all on `!recv_is_class`.
- **v0.5.594** — Closes #514: `s[0]` / `s.at(-1)` on a `(s: any)` parameter holding a string now correctly indexes via `js_dyn_index_get`; runtime tag-aware dispatch added for any-typed string methods.
- **v0.5.593** — Closes #510 (followup to #462): calling a method on a primitive whose name doesn't resolve now throws `TypeError: <kind>.<prop> is not a function` (was silent no-op).
- **v0.5.592** — Closes #471: `Record<number, T>` writes via `obj[i] = v` corrupted the heap — replace inline `obj+8+idx*8` store with type-dispatching `js_object_set_index_polymorphic`.
- **v0.5.591** — Refs #421/#420: cross-module `Call { callee: ExternFuncRef }` for an imported VARIABLE binding now fetches the closure value via the zero-arg getter and dispatches through `js_closure_callN`.
- **v0.5.590** — Closes #490: cross-platform system-tray icon API (`trayCreate`/`traySetIcon`/`traySetTooltip`/`trayAttachMenu`/`trayOnClick`/`trayDestroy`) on macOS / Windows / Linux + iOS/tvOS/visionOS/watchOS/Android stubs.
- **v0.5.589** — Closes #493: closure rest-param bundling now happens at runtime via a `func_ptr → fixed_arity` registry, fixing dynamic-dispatch call sites where codegen can't statically bundle.
- **v0.5.588** — Closes #507: perry-ext-net `Handle::current()` panic — auto-optimize now rebuilds tokio-using ext bindings into the same target dir as perry-stdlib so cargo unifies tokio compilations.
- **v0.5.587** — Closes #458/#491: WAV recording (`audioSetOutputFilename`/`audioStartRecording`/`audioStopRecording`) implemented on every UI backend (macOS/iOS/tvOS/visionOS/watchOS/Android/Windows; Linux+Web shipped in #458).
- **v0.5.586** — Closes #485: cross-module subclass field initializers + arrow class fields installed in parent ctor body now correctly inherit (unblocks hono `app.fetch` chain).
- **v0.5.585** — Refs #139: floating-point fast-math (`reassoc + contract` FMF) is now opt-in via `--fast-math` / `PERRY_FAST_MATH=1` / `package.json`. Default OFF means bit-exact f64 with Node on most code.
- **v0.5.584** — Closes #484: rest-parameter bundling for class method dispatch — parallel `method_has_rest` map populated alongside `method_param_counts`, threaded into the call site.


Older entries → CHANGELOG.md.

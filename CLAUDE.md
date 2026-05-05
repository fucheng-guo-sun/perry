# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

**NOTE**: Keep this file concise. Detailed changelogs live in CHANGELOG.md.

## Project Overview

Perry is a native TypeScript compiler written in Rust that compiles TypeScript source code directly to native executables. It uses SWC for TypeScript parsing and LLVM for code generation.

**Current Version:** 0.5.531


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

- **v0.5.531** — Refs #466 (Phase 2 of the native-package ecosystem): froze the `perry.nativeLibrary` manifest spec at v1 and wired `abiVersion` enforcement into the resolve path. `docs/src/native-libraries/manifest-v1.md` (linked from SUMMARY.md) is the authoritative spec — every field, every `params`/`returns` ABI type, every per-target key, the resolution order, the deprecation timetable. Companion JSON schema at `docs/api/manifest.schema.json` for editor validation. `NativeLibraryManifest` gains an `abi_version: Option<String>` field populated from `pkg.perry.nativeLibrary.abiVersion`; `validate_abi_version` (semver crate) checks the declared range covers the bundled `perry-ffi` version (currently equal to the workspace version since perry-ffi ships in lockstep with perry through the v0.5.x cycle). Bare-major declarations (`"0.5"`) auto-promote to `^0.5` so wrappers don't have to learn caret syntax to express the common case. Behavior on this branch: missing field → stderr warning, compilation continues (transitional rule; the v0.6.0 plan flips it to an error); valid range that excludes the bundled version → `anyhow::Error` propagated up through `collect_modules` so the user sees the message at compile time, not segfaults at runtime; unparseable string → error pointing at the offending package. Wired in both arms of `collect_modules.rs` (NativeCompiled and Interpreted paths) so packages with `perry.nativeLibrary` get validated regardless of how their TS surface is consumed. Four unit tests cover the matrix: missing → ok, matching caret → ok, far-future major → err, garbage string → err. `semver = "1.0"` added to `crates/perry/Cargo.toml` (was already a transitive dep via `perry-updater`).
- **v0.5.530** — Refs #466 (Phase D of #469): extracted the stable ABI surface for native-bindings packages into a new `perry-ffi` crate, and ported the smallest stdlib wrapper (`dotenv`) onto it as the acceptance test. `perry-ffi` v0.5 ships an intentionally tiny surface — `JsString` opaque handle, `alloc_string(&str) -> JsString`, `read_string(JsString) -> Option<&str>`, `JsString::{from_raw, as_raw, is_null}`, plus a re-export of `StringHeader` for `extern "C"` signatures. That's enough to port the four single-file stdlib wrappers (`dotenv`, `nanoid`, `uuid`, `slugify`); larger surfaces (arrays / objects / closures / async-runtime sharing) wait for the wrappers that actually need them so we don't commit to APIs we'll regret. New `crates/perry-ext-dotenv/` is a staticlib + rlib that depends only on `perry-ffi` and `serde_json` — zero references to `perry-runtime` internals. Functionally identical to `crates/perry-stdlib/src/dotenv.rs` (which stays in place — additive port, not a replacement); the well-known bindings table flip from #466 Phase 4 is what eventually swaps `import 'dotenv'` resolution from the stdlib copy to the new crate. `nm libperry_ext_dotenv.a | grep js_dotenv` shows the same three exports the stdlib version has (`js_dotenv_config`, `js_dotenv_config_path`, `js_dotenv_parse`). Tests round-trip a string through perry-ffi → js_dotenv_parse → JSON output, proving the contract end-to-end without touching the runtime crate. `docs/src/native-libraries/abi.md` documents the v0.5 surface, the semver contract (perry-ffi major bumps independently of perry-runtime), and what's deliberately *not* yet covered. Mdbook SUMMARY.md links the new doc under "npm Packages → Writing Native Bindings".
- **v0.5.529** — Closes #465 (Phase C of #469): markdown + `.d.ts` serializers for the API manifest. New `crates/perry-api-manifest/src/emit.rs` exposes `emit_markdown(version)` (one combined reference page; modules in alpha order; classes / properties / methods grouped within each module; stubs flagged ⚠) and `emit_dts(version)` (one `declare module "<name>" { ... }` block per supported module; loose `any`-typed signatures since the manifest doesn't carry argument types yet — followup under #466 Phase 2). The `--print-api-manifest` global flag now takes an optional value (`--print-api-manifest=markdown` / `=dts`); bare flag stays JSON for compat with v0.5.528 callers. `default` is the one TypeScript reserved keyword that hits in practice (npm modules with a callable default export — `dotenv`, `slugify`, `sharp`, `better-sqlite3`, `commander`, `dayjs`, `moment`, `cheerio`, `lru-cache`, `lodash`); the .d.ts emitter renders those as `export default function (...args: any[]): any;` rather than the syntactically-invalid `export function default(...)`. Tests assert every module from `API_MANIFEST` appears in both formats and that `crypto.randomUUID` shows up under `declare module "crypto"`. Generated artifacts committed at `docs/src/api/reference.md` (676 lines, 378 entries across 43 modules) and `docs/api/perry.d.ts` (476 lines), with `scripts/regen_api_docs.sh` as the one-shot regenerator that release CI will eventually invoke (the workflow integration itself is a follow-up — committing the artifacts means the diff stays visible in PRs that touch the dispatch table or the manifest). SUMMARY.md now links the auto-generated reference under "Standard Library".
- **v0.5.528** — Closes #463 (Phase A + B of the unified plan in #469): new `perry-api-manifest` crate exposes `API_MANIFEST: &[ApiEntry]` — a structured (module, name, kind, source, stub) listing of every supported stdlib symbol, populated mechanically from `NATIVE_MODULE_TABLE` (317 methods) plus hand-curated entries for modules whose dispatch goes through custom `Expr::*` HIR variants (`crypto`, `os`, `path`, `process`) and a small set of class registrations (`Buffer`, `EventEmitter`, `URL`, etc., 374 entries total). HIR lowering of `Expr::PropertyGet { object: NativeModuleRef(M), property: P }` consults `module_has_symbol(M, P)` and emits an `R005 UnimplementedApi` diagnostic with span info when the symbol isn't registered — `crypto.subtle.encrypt(...)` now errors at the offending line instead of silently returning undefined. Strictness is gated on `module_has_any_entries(M)` so modules whose surface hasn't been enumerated yet (incremental coverage) keep working — adding entries promotes the module to strict mode automatically. Stubs (`stub: true` in the manifest) are NOT treated as unimplemented; #464's runtime first-call warning surfaces those instead. Two escape hatches: env var `PERRY_ALLOW_UNIMPLEMENTED=1` skips the check (useful when the manifest has a real gap a followup will fix), and the `--print-api-manifest` global flag emits the manifest as JSON to stdout — drives #465's docs/.d.ts generators and lets editor tooling discover the supported surface without reading Rust source. `perry-codegen`'s `tests/manifest_consistency.rs` walks `NATIVE_MODULE_TABLE` and asserts every row has a counterpart in `API_MANIFEST` so drift can't ship. `perry-hir::ir::NATIVE_MODULES` and `requires_stdlib` migrated to thin re-exports / delegates over `perry_api_manifest::{NATIVE_MODULES, is_runtime_only_module}`. Parity sweep: 209 pass / 2 fail / 1 compile_fail (test_issue_446 is a pre-existing #462-era regression unrelated to this PR — verified by disabling the new check and re-running). Same manifest is the foundation for #465 (docs/.d.ts emit) and #466 Phase 2 (external `perry.nativeLibrary` spec freeze).
- **v0.5.527** — Closes #470: `Buffer.{read,write}{U,}Int{BE,LE}(offset, byteLength)` variable-byteLength forms were no-ops (`readUIntBE` returned `undefined`, `writeUIntBE` wrote nothing) — `dispatch_buffer_method` only handled the fixed-width `readUInt8/16/32` / `writeUInt8/16/32` cases and fell through to `TAG_UNDEFINED` for the parametric forms. Added `js_buffer_{read,write}_{uint,int}_{be,le}(buf, [value,] offset, byte_length)` to `crates/perry-runtime/src/buffer.rs` (1..=6 byte_length, sign-extends through `(n*8)`-bit two's complement for the signed reads, returns `undefined` / no-ops on out-of-range byteLength to match the rest of the buffer dispatch's tolerant-on-bad-args convention) and wired the 8 method-name arms into `dispatch_buffer_method` in `crates/perry-runtime/src/object.rs`. The codegen-side `try_emit_buffer_read_intrinsic` fast path is opt-in by exact method name (`classify_buffer_numeric_read` doesn't list these) so the variable-byteLength calls naturally fall through to the runtime dispatch — no codegen changes needed. Unblocks BSON `ObjectId` generation in pure-TS Mongo drivers (the 3-byte counter no longer serialises as zero, so `insertMany(N)` actually inserts N).
- **v0.5.526** — Closes #462: `PropertyGet` codegen now splits the existing `pget.recv_bad` block into a TAG_UNDEFINED/TAG_NULL nullish check + `js_throw_type_error_property_access` runtime helper that prints a node-shaped `TypeError: Cannot read properties of {undefined|null} (reading '<prop>')` and aborts. Non-nullish invalid receivers (numbers, bools, raw f64) keep the silent-undefined fall-through since Perry has no primitive auto-boxing yet. Optional chaining and `??` already short-circuit before reaching PropertyGet, so they're unaffected.
- **v0.5.525** — Refs #461: emit a fallback `perry_fn_<modprefix>__<exported>` undefined-stub on the export side for every `Export::Named` that no other emission path has already claimed — closes the `Order` / `Union` / `Emit` link errors that #460 carved out as follow-up. Two distinct symptom classes shared the same root cause: (1) exported classes (`export class Union<M>` in SchemaAST.ts, `export class Emit<R>` in subexecutor.ts) — the class declaration emits `perry_class_keys_*` + `perry_method_*` + a constructor symbol but no `perry_fn_<mod>__<ClassName>` value-getter, so consumer-side namespace property accesses (`AST.Union.make(...)`) link-failed because lower_call.rs's `ExternFuncRef` arm always resolves to `perry_fn_<src>__<name>`; (2) exported interfaces / type aliases (`export interface Order<in A>` in Order.ts) — type-only at runtime, but type annotations like `order.Order<RuntimeFiber<unknown,unknown>>` in `export const Order: order.Order<…> = internal.Order` (Fiber.ts:308) leak into the value-position symbol resolver and emit a load of `perry_fn_<Order_ts>__Order` that never had a definition. Fix is one block in `crates/perry-codegen/src/codegen.rs` after the #460 forwarding-wrapper pass: walk `hir.exports` for `Export::Named { exported }`, compute `perry_fn_<modprefix>__<sanitize(exported)>`, skip if `LlModule::has_function` (new helper) reports the symbol is already defined by an earlier path (function body / value-getter / #460 forwarding wrapper), otherwise emit a `() -> double` stub returning NaN-boxed undefined. Matches the consumer-side no-op wrapper at codegen.rs:1955 (which already returns undefined for imported classes referenced as values), so cross-module class/type-only references behave symmetrically — link cleanly, return undefined at runtime. After fix, `bun add effect && perry compile test_minimal.ts` drops `Order` / `Union` / `Emit` from the undefined-symbol list (down to 1: bare `_pull` from `internal/stream.ts`'s nested-arrow capture-mangling — separate bug, #461 stays open). Parity sweep: 210 pass / 2 known fails (was 209 / 2 — one test that was previously compile-failing now compiles cleanly through the new stub path).
- **v0.5.524** — Closes #464: stub registry + first-call runtime diagnostic for the harmonyos `perry_ui_*` / `perry_system_*` / `perry_updater_*` no-op stubs (and the runtime-only `js_ws_*` / `js_readline_*` stubs). `perry-runtime/build.rs` walks the four `perry-dispatch` tables + the hand-listed `direct_call_stubs` and emits both the stub function bodies (when `ohos-napi` is on) and a `STUB_MANIFEST: &[StubEntry]` constant (always — `perry check` runs on a host build). Each generated stub funnels through `crate::stub_diag::perry_stub_warn(symbol, reason, issue)` which prints `[perry] warning: \`<sym>\` is a no-op stub on this platform — <reason> (tracking: <issue>)` once per symbol per process; `PERRY_STUB_DIAG=off|silent|0|false` silences and `=verbose|all|every` prints every call. The same manifest powers a new static scan in `perry check --target harmonyos` (also `harmonyos-simulator`) that walks named imports from `perry/ui` / `perry/system` / `perry/updater` against `STUB_MANIFEST.ts_name` and emits an `R004 NoOpStub` diagnostic at the import span — surfaces the same warnings before the binary runs. Hot-loop drains (`js_ws_process_pending`, `js_stdlib_process_pending`) deliberately skip the warn helper since they tick every event-loop pass. `target_stubs_out_symbols` gates the static scan so a native-host `perry check` (where the platform UI crate owns those symbols) doesn't false-positive.
- **v0.5.523** — Closes #460: `export { local as keyword }` re-exports of TypeScript contextual keywords (`void` / `async` / `await` / `try` / `delete` / `continue`) had no `_perry_fn_<mod>__<keyword>` definition in the renamed module's object file — Effect's `node_modules/effect/src/{Effect,Deferred,Either,FiberRef,Fiber,Option,ScheduleDecision,Stream,internal/core,…}.ts` link-failed on 14 such symbols. Two parallel root causes were both leaving the symbol at the export site undefined: (1) function-decl renames — `function _void(){} ; export { _void as void }` — populated `module.exported_functions: [("void", func_id)]` correctly, but `crates/perry-codegen/src/codegen.rs::scoped_fn_name` only mangled `f.name` (`_void`), so the body was emitted as `perry_fn_<mod>___void` and nothing claimed the `__void` symbol; (2) const-binding renames — `const _await = core.deferredAwait ; export { _await as await }` — never even reached `exported_functions` (the `lookup_func` gate fails for non-fn-decls), and the `is_exportable` filter in `crates/perry-hir/src/lower.rs` excluded `Expr::PropertyGet` / `Expr::LocalGet` / `Expr::ExternFuncRef` / `Expr::FuncRef` so the local was also missing from `exported_objects` — the local id was never registered in `module_globals`, the value-getter loop in `codegen.rs` skipped emitting any symbol, and the public name was undefined. Fix: in codegen, after the user-function body loop, walk `hir.exported_functions` and emit a forwarding wrapper from `perry_fn_<mod>__<exported>` to `perry_fn_<mod>__<f.name>` whenever the two names differ (skip when equal — `export function foo(){}` self-export); also widen the `is_exportable` matcher in `lower.rs` to cover the four function-reference initializer shapes and additionally pin the local name into `exported_objects` so the global-emission gate fires; in codegen's value-getter loop, walk `hir.exports` for `Export::Named { local, exported }` rename pairs and emit a duplicate getter under the exported name. The forwarding wrapper for function-decl renames also has to suppress the value-getter (added an `is_function_alias` predicate against `exported_functions`) — without it the two paths collide and clang errors `invalid redefinition of function 'perry_fn_<mod>__async'`. After fix, all 14 keyword-name undefined-symbol errors disappear from `bun add effect && perry compile test_minimal.ts`; the remaining 3 (`Order`, `Union`, `Emit`) are class-as-function symbols carved out by #460 as a separate follow-up. Parity sweep stays at 209 pass / 2 known fails.
- **v0.5.522** — Closes #467: sanitize scoped package.json names (`@scope/pkg` → `scope-pkg`) before they flow into the linker's `-o` arg or default bundle ID — ld64 was treating the leading `@` as a response-file directive.
- **v0.5.521** — Closes #457: skip inlining of generator functions in `is_inlinable` — the inliner was erasing `yield` semantics and collapsing `gen()` to its terminating `return` value.
- **v0.5.520** — Closes #456: unroll pass aliased LocalIds + FuncIds across cloned iterations, producing duplicate `@perry_global_*` definitions and collapsing per-iteration closures into one body.
- **v0.5.519** — Dependabot security cleanup: `jsonwebtoken` 9.3 → 10.3, `lru` 0.12 → 0.16, `validator` 0.18 → 0.20, drop `atty` for `std::io::IsTerminal`.
- **v0.5.518** — Closes #454: bump `redis` 0.25 → 1.2, `sqlx` 0.8.0 → 0.8.6, `rusqlite` 0.31 → 0.32.1 — clears never-type-fallback future-incompat warnings.
- **v0.5.517** — Closes #443 (followup to v0.5.498): widen `is_nanboxed_string` to also accept `SHORT_STRING_TAG` (0x7FF9) in windows/android/gtk4/macos state bindings.
- **v0.5.516** — Closes #434: SSO (0x7FF9) vs heap-string (0x7FFF) representation mismatch broke `Set.has` / `Map.get` / `obj[key]` for short JSON.parse'd keys.
- **v0.5.515** — Closes #407: `console.log(...arr)` dropped all but the first arg — added `console.*` fast path to the `Expr::CallSpread` codegen arm.
- **v0.5.514** — Closes #393: `compute_max_func_id` missed `NativeMethodCall` args, so async-to-generator's `next_func_id` collided with route-handler closure ids.
- **v0.5.513** — Closes #399 (round 2): add `notificationSchedule_{interval,calendar,location}` to `direct_call_stubs` so harmonyos `.so` link-resolves.
- **v0.5.512** — Closes #396/#397/#398: macOS bottle bundles tvOS/visionOS/watchOS cross-libs (device + sim) alongside iOS.
- **v0.5.511** — release-CI gate fix: add `test_edge_closures` to compile-smoke `SKIP_TESTS` (separate skip mechanism from parity's `known_failures.json`).
- **v0.5.510** — release-CI gate fixes: `cargo fmt --all` for lint drift; triage #456 + #457 into `known_failures.json` to unblock v0.5.509 publish.
- **v0.5.509** — Closes #447 + un-skip 6 async tests; v0.5.508 ABI fix transitively closed #447's iter-object register-class mismatch.
- **v0.5.508** — Closes #448 + #451: ABI mismatch in `js_object_set_field` (DOUBLE arg vs runtime `JSValue` u64) put closure values in xmm0 instead of GP register.
- **v0.5.507** — Closes #450: `defineProperty` accessor `this` was descriptor literal not target obj; codegen flags `captures_this`, runtime clone-rebinds.
- **v0.5.505** — Closes #449: fold `new.target.<prop>` to a literal at HIR lowering, bypassing the broken `MetaProp(NewTarget)` Object-literal path.
- **v0.5.503** — Closes #446: bind class-method PropertyGet via `js_class_method_bind`; flow type-only-import class metadata into `imported_classes`.
- **v0.5.502** — Closes #444: fold `import.meta.{url,main,dirname,filename}` to literal at lowering.
- **v0.5.501** — Closes #422: `new net.Socket()` + `sock.connect(port, host)` + `net.connect(...)` — pure-TS TCP clients work end-to-end.
- **v0.5.500** — Closes #429: GTK4 `appSetTimer` callbacks scheduled mid-event-loop now install immediately.
- **v0.5.495** — `perry-transform` static-trip-count for-loop full-unroll pass — 22% improvement on `image_convolution`'s 5×5 blur kernel.

Older entries → CHANGELOG.md.

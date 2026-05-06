//! Auto-rebuild perry-runtime + perry-stdlib with the smallest matching
//! Cargo feature set so the compiled `.o` only links the runtime APIs
//! the user's TS code actually uses.
//!
//! Tier 2.1 follow-up (v0.5.341) — extracts `OptimizedLibs` + the
//! `build_optimized_libs` driver from `compile.rs`. ~390 LOC of
//! self-contained library-build orchestration. Both `runtime` and
//! `stdlib` halves fall back to the prebuilt libraries gracefully on
//! any failure (no source on disk, no cargo, build error). Cargo's
//! incremental cache is keyed per (target dir, feature set), and we
//! use a hash-keyed target dir so consecutive runs with the same
//! profile are no-ops after the first build.

use std::path::PathBuf;
use std::process::Command;

use crate::commands::stdlib_features::{compute_required_features, features_to_cargo_arg};
use crate::OutputFormat;

use super::library_search::{find_harmonyos_sdk, harmonyos_cross_env};
use super::{find_perry_workspace_root, rust_target_triple, CompilationContext};

pub struct OptimizedLibs {
    /// Path to the rebuilt `libperry_runtime.a` (or `perry_runtime.lib`).
    /// `None` means "fall back to the prebuilt one in target/release/".
    pub runtime: Option<PathBuf>,
    /// Path to the rebuilt `libperry_stdlib.a`. `None` means "fall back
    /// to the prebuilt full stdlib".
    pub stdlib: Option<PathBuf>,
    /// LLVM bitcode (`.bc`) for perry-runtime (Phase J).
    pub runtime_bc: Option<PathBuf>,
    /// LLVM bitcode (`.bc`) for perry-stdlib (Phase J).
    pub stdlib_bc: Option<PathBuf>,
    /// LLVM bitcode (`.bc`) for additional crates (UI, jsruntime, geisterhand).
    pub extra_bc: Vec<PathBuf>,
    /// Extra `.a` archives to add to the link line — one per
    /// well-known native binding (#466 Phase 4) that the compile
    /// pipeline routed away from the perry-stdlib copy. Whenever an
    /// entry is added here, the corresponding perry-stdlib feature
    /// is *also* stripped from the rebuild so the link line stays
    /// free of duplicate `_js_*` symbols.
    pub well_known_libs: Vec<PathBuf>,
}

impl OptimizedLibs {
    pub(super) fn empty() -> Self {
        OptimizedLibs {
            runtime: None,
            stdlib: None,
            runtime_bc: None,
            stdlib_bc: None,
            extra_bc: Vec::new(),
            well_known_libs: Vec::new(),
        }
    }
}

/// Rebuild perry-runtime + perry-stdlib in a single cargo invocation with
/// the chosen Cargo features and panic mode, and return paths to the
/// resulting archives. Both halves fall back to the prebuilt libraries
/// gracefully on any failure (no source on disk, no cargo, build error).
///
/// This is the auto-mode workhorse — it lets the compile driver pick the
/// smallest matching profile for the user's TS code without any manual
/// flags. Cargo's incremental cache is keyed per (target dir, feature
/// set), and we use a hash-keyed target dir so consecutive runs with the
/// same profile are no-ops after the first build.
pub(super) fn build_optimized_libs(
    ctx: &CompilationContext,
    target: Option<&str>,
    cli_features: &[String],
    format: OutputFormat,
    verbose: u8,
) -> OptimizedLibs {
    // (compute_required_features + features_to_cargo_arg imported at module top)
    let mut features = compute_required_features(
        &ctx.native_module_imports,
        ctx.uses_fetch,
        ctx.uses_crypto_builtins,
    );

    // #466 Phase 4 step 2: well-known bindings flip. For each
    // imported module that has an entry in `well_known_bindings.toml`
    // *and* whose bundled `.a` is on disk, drop the corresponding
    // perry-stdlib feature so the rebuild stops emitting that
    // module's symbols, then queue the bundled `.a` to be added to
    // the link line. Net result: the program links against the
    // external wrapper instead of the perry-stdlib copy, with no
    // duplicate-symbol risk.
    //
    // **Default-on as of v0.5.573** — Phase 5 dogfood completed in
    // v0.5.572 (34 perry-ext-* wrappers covering every previously
    // in-tree binding). The env-var gate (`PERRY_USE_WELL_KNOWN=1`)
    // that gated the introductory cycle is now inverted:
    // `PERRY_DISABLE_WELL_KNOWN=1` reverts to perry-stdlib's
    // copies for bisection. If a bundled `.a` is missing on disk,
    // each entry falls back to the perry-stdlib copy individually
    // (logged with `well-known: skipping` when verbose), so a
    // partially-built workspace still produces a working binary.
    let use_well_known = std::env::var_os("PERRY_DISABLE_WELL_KNOWN").is_none();
    let mut well_known_libs: Vec<PathBuf> = Vec::new();
    if use_well_known {
        for module in &ctx.native_module_imports {
            let Some(binding) = super::well_known::lookup_well_known(module) else {
                continue;
            };
            // Locate the bundled `.a`. Workspace root might be unset
            // when perry runs from a release tarball — in that case
            // the bundled `.a` is expected to live next to the
            // perry binary itself; followups under #466 Phase 4
            // teach `bundled_staticlib_path` about that layout.
            let workspace_root_opt = find_perry_workspace_root();
            let Some(workspace_root) = workspace_root_opt.as_ref() else {
                continue;
            };
            let Some(lib_path) = super::well_known::bundled_staticlib_path(workspace_root, binding)
            else {
                if matches!(format, OutputFormat::Text) && verbose > 0 {
                    eprintln!(
                        "  well-known: skipping `{}` — bundled `lib{}.a` not found \
                         in target/release; falling back to perry-stdlib copy.",
                        module, binding.lib
                    );
                }
                continue;
            };
            // Strip the perry-stdlib feature(s) this binding was
            // covering. `module_to_features` is the same table
            // `compute_required_features` consulted above, so we
            // know exactly what to remove.
            for feat in crate::commands::stdlib_features::module_to_features(module) {
                features.remove(*feat);
            }
            // perry-ffi's async surface (#466 Phase 1.1 / Phase 5
            // step 5+) is gated behind perry-stdlib's
            // `async-runtime` feature — the `perry_ffi_*` shim
            // module that wrappers like bcrypt / argon2 / ws / db
            // pull through linking lives in
            // `crates/perry-stdlib/src/perry_ffi_async.rs` and
            // can only be compiled when tokio is in the build.
            // Stripping `bundled-bcrypt` (etc.) without
            // re-asserting `async-runtime` would leave the
            // wrapper's `.a` carrying unresolved `perry_ffi_*`
            // references. Detect async wrappers by checking
            // whether the original feature list contained an
            // async feature; if it did, ensure it stays.
            let original_features = crate::commands::stdlib_features::module_to_features(module);
            if original_features.iter().any(|f| {
                matches!(
                    *f,
                    "bundled-bcrypt"
                        | "bundled-argon2"
                        | "bundled-nodemailer"
                        | "bundled-ioredis"
                        | "bundled-pg"
                        | "bundled-mysql2"
                        | "bundled-mongodb"
                        | "bundled-ws"
                        | "bundled-net"
                        | "http-client"
                        | "bundled-streams"
                        | "bundled-fastify"
                )
            }) {
                features.insert("async-runtime");
            }
            // v0.5.579 — when the flip strips `bundled-net`, activate
            // `external-net-pump` so perry-stdlib's
            // `js_stdlib_process_pending` knows to call into
            // perry-ext-net's queue. Without this the call site is
            // `#[cfg]`-gated off and tokio events stay queued forever.
            if original_features.contains(&"bundled-net") {
                features.insert("external-net-pump");
            }
            if matches!(format, OutputFormat::Text) {
                println!(
                    "  well-known: routing `{}` → {} ({})",
                    module,
                    lib_path.display(),
                    binding.tracking.as_deref().unwrap_or("no tracking issue")
                );
            }
            well_known_libs.push(lib_path);
        }
    }

    // The UI backends (perry-ui-gtk4 on Linux, perry-ui-macos, perry-ui-windows)
    // reach into perry-stdlib's async bridge from GLib/NSTimer/WM_TIMER
    // trampolines (js_stdlib_process_pending, js_promise_run_microtasks).
    // Those symbols live in perry-stdlib/src/common/async_bridge.rs which is
    // gated on `#[cfg(feature = "async-runtime")]`. For a bare UI program
    // whose user code imports zero stdlib modules, compute_required_features
    // returns an empty set and the auto-optimized stdlib is built with
    // --no-default-features — no `async-runtime`, no async_bridge module, no
    // symbol. Force `async-runtime` whenever the program pulls in a UI
    // backend so the trampolines resolve at link time.
    if ctx.needs_ui {
        features.insert("async-runtime");
    }
    let feature_arg = features_to_cargo_arg(&features);

    // panic = "abort" is safe whenever no `catch_unwind` callers are
    // reachable. Today those live in:
    //   - perry-runtime/src/thread.rs (perry/thread `spawn`)
    //   - perry-ui-{macos,ios}/* (UI callback isolation)
    //   - perry-runtime plugin host (`needs_plugins` → -rdynamic +
    //     -force_load paths that may rely on unwind tables for plugin
    //     dylibs)
    //   - geisterhand registry callbacks
    // Whenever the user binary doesn't pull any of those in, switching
    // to `abort` saves ~12-18 % off the final binary by dropping
    // __TEXT,__eh_frame, __TEXT,__gcc_except_tab, __TEXT,__unwind_info
    // and the matching landing pads / Drop glue.
    let panic_abort_safe =
        !ctx.needs_ui && !ctx.needs_thread && !ctx.needs_plugins && !ctx.needs_geisterhand;

    // Locate the workspace. Without source we can't rebuild — fall back
    // to whatever's prebuilt next to perry on disk. The fallback names are
    // platform-specific so the log doesn't claim Perry is searching for a
    // `.a` on Windows (it isn't — `find_runtime_library` / `find_stdlib_library`
    // route to `perry_runtime.lib` + `perry_stdlib.lib` on Windows hosts).
    let workspace_root = match find_perry_workspace_root() {
        Some(p) => p,
        None => {
            if matches!(format, OutputFormat::Text) && verbose > 0 {
                let (rt_name, std_name) = match target {
                    Some("windows") => ("perry_runtime.lib", "perry_stdlib.lib"),
                    None if cfg!(target_os = "windows") => {
                        ("perry_runtime.lib", "perry_stdlib.lib")
                    }
                    _ => ("libperry_runtime.a", "libperry_stdlib.a"),
                };
                eprintln!(
                    "  auto-optimize: Perry workspace source not found, \
                     using prebuilt {} + {}",
                    rt_name, std_name
                );
            }
            return OptimizedLibs::empty();
        }
    };

    // Hash the (features, panic_mode, target) tuple into the target dir
    // name so cargo treats each combination as its own incremental cache.
    // Cheap djb2 — no need for the SipHash overhead.
    let target_str = target.unwrap_or("host");
    let key_input = format!("{}|{}|{}", feature_arg, panic_abort_safe, target_str);
    let mut hash: u64 = 5381;
    for b in key_input.as_bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(*b as u64);
    }
    let target_dir = workspace_root.join(format!("target/perry-auto-{:016x}", hash));

    if matches!(format, OutputFormat::Text) {
        let panic_str = if panic_abort_safe { "abort" } else { "unwind" };
        let feat_str = if features.is_empty() {
            "(no optional features)".to_string()
        } else {
            feature_arg.clone()
        };
        println!(
            "  auto-optimize: rebuilding runtime+stdlib (panic={}, features={})",
            panic_str, feat_str
        );
    }

    // Tier-3 Apple targets (tvOS, watchOS) aren't shipped with a prebuilt
    // libstd; cargo needs `+nightly -Zbuild-std` to synthesize core/alloc/std
    // from source for the cross-compile.
    let is_tier3 = matches!(
        target,
        Some("tvos") | Some("tvos-simulator") | Some("watchos") | Some("watchos-simulator")
    );

    let mut cargo_cmd = Command::new("cargo");
    if is_tier3 {
        cargo_cmd.arg("+nightly");
    }
    cargo_cmd
        .current_dir(&workspace_root)
        .env("CARGO_TARGET_DIR", &target_dir)
        .arg("build")
        .arg("--release")
        .arg("-p")
        .arg("perry-runtime")
        .arg("-p")
        .arg("perry-stdlib")
        .arg("--no-default-features");
    if is_tier3 {
        cargo_cmd.arg("-Zbuild-std=std,panic_abort");
    }
    // Both perry-runtime and perry-stdlib accept their own feature lists.
    // Cargo's `--features` takes `crate/feature` syntax for cross-crate
    // selection — we always enable perry-stdlib's stdlib-side bridge so
    // perry-runtime exports the right symbols, and the user-derived
    // stdlib features.
    let mut cross_features: Vec<String> = vec![
        // perry-runtime's "full" feature gates plugin + os.hostname/homedir.
        // Auto-mode keeps it on so existing behavior is preserved; the
        // panic mode is what shrinks the binary.
        "perry-runtime/full".to_string(),
    ];
    for f in &features {
        cross_features.push(format!("perry-stdlib/{}", f));
    }
    // CLI `--features` values that target the runtime (game-loop entry-point
    // shims gated behind `ios-game-loop` / `watchos-game-loop` in
    // `perry-runtime/Cargo.toml`) need `perry-runtime/<f>` passed through, not
    // `perry-stdlib/<f>` — they gate a Rust module, not an npm dep surface.
    for f in cli_features {
        if f == "ios-game-loop" || f == "watchos-game-loop" || f == "ohos-napi" {
            cross_features.push(format!("perry-runtime/{}", f));
        }
    }
    if !cross_features.is_empty() {
        cargo_cmd.arg("--features").arg(cross_features.join(","));
    }
    if let Some(triple) = rust_target_triple(target) {
        cargo_cmd.arg("--target").arg(triple);
    }
    // HarmonyOS cross-compile needs the OHOS SDK's clang on PATH for C
    // dependencies (notably libmimalloc-sys) — without --sysroot the build
    // fails in build.rs with "'pthread.h' file not found".
    if matches!(target, Some("harmonyos") | Some("harmonyos-simulator")) {
        match find_harmonyos_sdk() {
            Some(sdk) => {
                for (k, v) in harmonyos_cross_env(&sdk, target) {
                    cargo_cmd.env(k, v);
                }
            }
            None => {
                if matches!(format, OutputFormat::Text) {
                    eprintln!(
                        "  auto-optimize: OHOS SDK not found — set OHOS_SDK_HOME to the DevEco Studio \
                         SDK root (the dir containing native/llvm/bin/clang). Skipping auto-optimize."
                    );
                }
                return OptimizedLibs::empty();
            }
        }
    }
    if panic_abort_safe {
        // Override the workspace profile's `panic = "unwind"` for the
        // duration of this invocation. RUSTFLAGS is the only path that
        // works without a custom cargo profile, and cargo correctly
        // reuses incremental artifacts that were built with the same
        // RUSTFLAGS. The hash-keyed CARGO_TARGET_DIR keeps the abort
        // and unwind builds from clobbering each other's cache.
        cargo_cmd.env("RUSTFLAGS", "-C panic=abort");
    }

    // Closes #25 (the v0.5.384 NJOBS 6→3 retreat): serialize parallel
    // `perry compile` invocations that target the SAME `target/perry-auto
    // -<hash>` directory via an OS-level file lock. Cargo has its own
    // target-dir lock (`.cargo-lock`) that prevents concurrent COMPILES,
    // but the FILE OUTPUT is rename'd at link end — meaning worker B's
    // clang can read `libperry_runtime.a` while worker A's cargo is
    // mid-rename and see errno=2. The race window is sub-second but
    // fired reliably at NJOBS=6 on the macos-14 compile-smoke runner.
    //
    // The lock is per-hash, so different feature combos still build in
    // parallel. fslock is portable (flock on Unix, LockFileEx on
    // Windows) and was already a transitive dep — no new crate cost.
    //
    // Best-effort: if the dir create or lock acquisition fails for any
    // reason, fall through and run cargo unguarded. The retry loop in
    // the smoke script's compile_one already handles the residual race
    // window if any worker still slips through.
    let _build_lock = {
        let _ = std::fs::create_dir_all(&target_dir);
        let lock_path = target_dir.join(".perry-auto-build.lock");
        match fslock::LockFile::open(&lock_path) {
            Ok(mut lf) => {
                let _ = lf.lock();
                Some(lf)
            }
            Err(_) => None,
        }
    };

    let status = match cargo_cmd.status() {
        Ok(s) => s,
        Err(e) => {
            if matches!(format, OutputFormat::Text) {
                eprintln!(
                    "  auto-optimize: failed to spawn cargo ({}), \
                     using prebuilt libraries",
                    e
                );
            }
            return OptimizedLibs::empty();
        }
    };
    if !status.success() {
        if matches!(format, OutputFormat::Text) {
            eprintln!(
                "  auto-optimize: cargo build failed (exit {}), \
                 using prebuilt libraries",
                status
            );
        }
        return OptimizedLibs::empty();
    }

    // Resolve both archive paths.
    let runtime_name = match target {
        Some("windows") => "perry_runtime.lib",
        #[cfg(target_os = "windows")]
        None => "perry_runtime.lib",
        _ => "libperry_runtime.a",
    };
    let stdlib_name = match target {
        Some("windows") => "perry_stdlib.lib",
        #[cfg(target_os = "windows")]
        None => "perry_stdlib.lib",
        _ => "libperry_stdlib.a",
    };
    let release_dir = if let Some(triple) = rust_target_triple(target) {
        target_dir.join(triple).join("release")
    } else {
        target_dir.join("release")
    };
    let runtime_path = release_dir.join(runtime_name);
    let stdlib_path = release_dir.join(stdlib_name);

    if matches!(format, OutputFormat::Text) {
        if let Ok(meta) = std::fs::metadata(&runtime_path) {
            println!(
                "  auto-optimize: built {} ({:.1} MB)",
                runtime_path.display(),
                meta.len() as f64 / (1024.0 * 1024.0)
            );
        }
        if let Ok(meta) = std::fs::metadata(&stdlib_path) {
            println!(
                "  auto-optimize: built {} ({:.1} MB)",
                stdlib_path.display(),
                meta.len() as f64 / (1024.0 * 1024.0)
            );
        }
    }

    // Phase J: when PERRY_LLVM_BITCODE_LINK=1, also emit LLVM bitcode
    // (.bc) for whole-program LTO via `cargo rustc --emit=llvm-bc,link`.
    let bitcode_requested = std::env::var("PERRY_LLVM_BITCODE_LINK").ok().as_deref() == Some("1");
    let (runtime_bc, stdlib_bc, extra_bc) = if bitcode_requested {
        if matches!(format, OutputFormat::Text) {
            println!("  auto-optimize: emitting LLVM bitcode for whole-program LTO");
        }

        let mut bc_rustflags = String::new();
        if panic_abort_safe {
            bc_rustflags.push_str("-C panic=abort ");
        }
        bc_rustflags.push_str("-C codegen-units=1");

        let emit_bc = |crate_name: &str| -> Option<PathBuf> {
            let mut cmd = Command::new("cargo");
            cmd.current_dir(&workspace_root)
                .env("CARGO_TARGET_DIR", &target_dir)
                .env("RUSTFLAGS", &bc_rustflags)
                .arg("rustc")
                .arg("--release")
                .arg("-p")
                .arg(crate_name)
                .arg("--no-default-features");
            if !cross_features.is_empty() {
                cmd.arg("--features").arg(cross_features.join(","));
            }
            if let Some(triple) = rust_target_triple(target) {
                cmd.arg("--target").arg(triple);
            }
            cmd.arg("--").arg("--emit=llvm-bc,link");

            match cmd.status() {
                Ok(s) if s.success() => {}
                Ok(s) => {
                    if matches!(format, OutputFormat::Text) {
                        eprintln!(
                            "  auto-optimize: cargo rustc --emit=llvm-bc for {} failed (exit {})",
                            crate_name, s
                        );
                    }
                    return None;
                }
                Err(e) => {
                    if matches!(format, OutputFormat::Text) {
                        eprintln!(
                            "  auto-optimize: failed to spawn cargo rustc for {} ({})",
                            crate_name, e
                        );
                    }
                    return None;
                }
            }

            // Glob for the .bc file in deps/
            let deps_dir = release_dir.join("deps");
            let crate_underscore = crate_name.replace('-', "_");
            let mut candidates: Vec<PathBuf> = Vec::new();
            if let Ok(entries) = std::fs::read_dir(&deps_dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name_str = name.to_string_lossy();
                    if name_str.starts_with(&format!("{}-", crate_underscore))
                        && name_str.ends_with(".bc")
                        && !name_str.contains(".rcgu")
                    {
                        candidates.push(entry.path());
                    }
                }
            }
            candidates.sort_by(|a, b| {
                let ma = a.metadata().and_then(|m| m.modified()).ok();
                let mb = b.metadata().and_then(|m| m.modified()).ok();
                mb.cmp(&ma)
            });
            if let Some(bc_path) = candidates.first() {
                if matches!(format, OutputFormat::Text) {
                    if let Ok(meta) = std::fs::metadata(bc_path) {
                        println!(
                            "  auto-optimize: bitcode {} ({:.1} MB)",
                            bc_path.display(),
                            meta.len() as f64 / (1024.0 * 1024.0)
                        );
                    }
                }
                Some(bc_path.clone())
            } else {
                if matches!(format, OutputFormat::Text) {
                    eprintln!(
                        "  auto-optimize: no .bc file found for {} in {}",
                        crate_name,
                        deps_dir.display()
                    );
                }
                None
            }
        };

        let rt_bc = emit_bc("perry-runtime");
        let sl_bc = emit_bc("perry-stdlib");

        // Emit .bc for additional crates (UI, jsruntime, geisterhand).
        // HarmonyOS has no `perry-ui-harmonyos` crate by design — UI is
        // emitted as ArkUI source via the codegen-arkts harvest, and
        // any `perry_ui_*` / `perry_system_*` / `perry_updater_*` symbols
        // that survive into the .so resolve via the no-op stubs auto-
        // generated by `perry-runtime/build.rs` (#395 + #399). The
        // harmonyos branch in compile.rs unconditionally clears
        // `needs_ui` for that target so we never reach this match arm
        // with `Some("harmonyos*")`.
        let mut extra = Vec::new();
        if ctx.needs_ui {
            let ui_crate = match target {
                Some("ios-simulator")
                | Some("ios")
                | Some("ios-widget")
                | Some("ios-widget-simulator") => "perry-ui-ios",
                Some("visionos-simulator") | Some("visionos") => "perry-ui-visionos",
                Some("android") => "perry-ui-android",
                Some("watchos-simulator") | Some("watchos") => "perry-ui-watchos",
                Some("tvos-simulator") | Some("tvos") => "perry-ui-tvos",
                Some("linux") => "perry-ui-gtk4",
                Some("windows") => "perry-ui-windows",
                Some("macos") => "perry-ui-macos",
                _ => {
                    if cfg!(target_os = "linux") {
                        "perry-ui-gtk4"
                    } else {
                        "perry-ui-macos"
                    }
                }
            };
            if let Some(bc) = emit_bc(ui_crate) {
                extra.push(bc);
            }
        }
        if ctx.needs_geisterhand {
            if let Some(bc) = emit_bc("perry-ui-geisterhand") {
                extra.push(bc);
            }
        }
        if ctx.needs_js_runtime {
            if let Some(bc) = emit_bc("perry-jsruntime") {
                extra.push(bc);
            }
        }

        (rt_bc, sl_bc, extra)
    } else {
        (None, None, Vec::new())
    };

    OptimizedLibs {
        runtime: if runtime_path.exists() {
            Some(runtime_path)
        } else {
            None
        },
        stdlib: if stdlib_path.exists() {
            Some(stdlib_path)
        } else {
            None
        },
        runtime_bc,
        stdlib_bc,
        extra_bc,
        well_known_libs,
    }
}

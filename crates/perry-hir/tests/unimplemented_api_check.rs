//! Issue #463 — references to stdlib symbols Perry doesn't implement
//! must error at HIR lowering with a span pointing at the offending
//! property access. Companion to #464 (runtime first-call warnings on
//! intentional no-op stubs); this test covers the *not implemented at
//! all* case.

use perry_diagnostics::SourceCache;
use perry_hir::lower_module;
use perry_parser::parse_typescript_with_cache;

fn lower_result(src: &str) -> Result<perry_hir::Module, String> {
    let src = src.to_string();
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(move || {
            let mut cache = SourceCache::new();
            let parsed = parse_typescript_with_cache(&src, "test.ts", &mut cache)
                .expect("parse should succeed");
            lower_module(&parsed.module, "test", "test.ts").map_err(|e| e.to_string())
        })
        .expect("spawn lower thread")
        .join()
        .expect("lower thread panicked")
}

/// The motivating example from #463: `crypto.subtle` doesn't exist in
/// Perry's `crypto` surface, so accessing it must error.
#[test]
fn crypto_subtle_is_rejected() {
    let result = lower_result(
        r#"
        import * as crypto from "crypto";
        const ct = crypto.subtle;
    "#,
    );
    let err = result.expect_err("expected compile error for crypto.subtle");
    assert!(
        err.contains("crypto.subtle") && err.contains("not implemented"),
        "expected error naming `crypto.subtle` and `not implemented`, got: {err}"
    );
    // Pointer to the followup machinery / docs.
    assert!(
        err.contains("#463") || err.contains("--print-api-manifest"),
        "expected error to reference issue or escape hatch, got: {err}"
    );
}

/// `node:` prefix doesn't change the answer.
#[test]
fn node_prefix_subtle_is_rejected() {
    let result = lower_result(
        r#"
        import * as crypto from "node:crypto";
        const ct = crypto.subtle;
    "#,
    );
    assert!(result.is_err(), "node:crypto.subtle should error too");
}

/// Implemented `crypto` methods continue to compile.
#[test]
fn crypto_random_uuid_is_accepted() {
    let result = lower_result(
        r#"
        import * as crypto from "crypto";
        const id = crypto.randomUUID();
    "#,
    );
    assert!(
        result.is_ok(),
        "crypto.randomUUID() must continue to compile: {result:?}"
    );
}

/// Modules that have at least one manifest entry are strict — any
/// unknown property errors. `crypto.foobar` is not implemented.
#[test]
fn crypto_unknown_method_is_rejected() {
    let result = lower_result(
        r#"
        import * as crypto from "crypto";
        const x = crypto.foobar;
    "#,
    );
    assert!(result.is_err(), "crypto.foobar should error");
}

/// `os.platform` is implemented; `os.totalmem` too. Both must continue
/// to compile so the gap suite stays green.
#[test]
fn os_implemented_methods_compile() {
    let result = lower_result(
        r#"
        import * as os from "os";
        const p = os.platform;
        const m = os.totalmem;
    "#,
    );
    assert!(
        result.is_ok(),
        "os.platform / os.totalmem must continue to compile: {result:?}"
    );
}

/// Constants like `os.EOL` and `path.sep` are registered as Property
/// entries, not methods. They must compile.
#[test]
fn os_eol_and_path_sep_compile() {
    let result = lower_result(
        r#"
        import * as os from "os";
        import * as path from "path";
        const eol = os.EOL;
        const sep = path.sep;
    "#,
    );
    assert!(
        result.is_ok(),
        "os.EOL / path.sep must continue to compile: {result:?}"
    );
}

// PERRY_ALLOW_UNIMPLEMENTED=1 escape hatch is intentionally not unit-
// tested here: env vars are process-global and would race with the
// other tests' positive-error assertions in the same test binary.
// The escape is exercised end-to-end via `perry compile` integration
// (where each invocation is its own process) — see the README/docs
// added in this PR for the manual verification steps.

/// Modules with zero entries in the manifest fall through to existing
/// permissive behavior. This lets coverage land incrementally without
/// breaking unrelated working code.
///
/// `axios` is in `NATIVE_MODULES` but currently has no entries in the
/// manifest. Reading an arbitrary property must NOT error.
#[test]
fn module_with_no_manifest_entries_is_permissive() {
    let result = lower_result(
        r#"
        import axios from "axios";
        const x = axios.foo;
    "#,
    );
    assert!(
        result.is_ok(),
        "axios.foo must compile while axios has no manifest entries: {result:?}"
    );
}

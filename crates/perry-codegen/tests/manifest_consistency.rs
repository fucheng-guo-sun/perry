//! Drift guard for `perry-api-manifest::API_MANIFEST` (#463).
//!
//! Every row of `NATIVE_MODULE_TABLE` (the static dispatch table in
//! `lower_call.rs`) must have a counterpart entry in `API_MANIFEST`,
//! otherwise the unimplemented-API check would error on a real
//! implementation. This test walks both and reports any mismatch.
//!
//! Class-filtered duplicates collapse to one manifest entry — the
//! manifest tracks "is this method known on this module?", not the
//! per-class signature variants.

use perry_api_manifest::{ApiKind, API_MANIFEST};
use perry_codegen::iter_native_method_signatures;

#[test]
fn every_dispatch_entry_has_manifest_counterpart() {
    let mut missing: Vec<String> = Vec::new();

    for sig in iter_native_method_signatures() {
        // Look for a manifest entry on the same (module, name) where
        // the kind is Method with matching has_receiver. class_filter
        // mismatches across rows of the same (module, method) pair are
        // expected — the dispatch table specializes by class, the
        // manifest does not.
        let hit = API_MANIFEST.iter().any(|e| {
            e.module == sig.module
                && e.name == sig.method
                && matches!(
                    e.kind,
                    ApiKind::Method { has_receiver, .. } if has_receiver == sig.has_receiver
                )
        });
        if !hit {
            let cls = sig.class_filter.unwrap_or("-");
            missing.push(format!(
                "{}::{} (has_receiver={}, class_filter={})",
                sig.module, sig.method, sig.has_receiver, cls
            ));
        }
    }

    assert!(
        missing.is_empty(),
        "API_MANIFEST is missing {} entry/entries that exist in NATIVE_MODULE_TABLE:\n  {}\n\n\
         Add the missing rows to crates/perry-api-manifest/src/entries.rs — \
         drift here would make the unimplemented-API check (#463) error on real implementations.",
        missing.len(),
        missing.join("\n  ")
    );
}

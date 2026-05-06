//! Markdown + TypeScript-declaration serializers for `API_MANIFEST`.
//!
//! Closes the docs / `.d.ts` half of #465. The compiler's
//! `--print-api-manifest=markdown` and `--print-api-manifest=dts` flags
//! delegate to these. Output is deterministic — modules sort
//! alphabetically, entries within a module sort by kind then name —
//! so regenerated docs produce stable diffs in CI.

use crate::{ApiEntry, ApiKind, ApiSource, API_MANIFEST};
use std::collections::BTreeMap;
use std::fmt::Write;

/// Render the manifest as a single combined Markdown reference page.
/// Compiler version is interpolated into the header so consumers can
/// tell at a glance which Perry release the doc was generated from.
pub fn emit_markdown(perry_version: &str) -> String {
    let mut out = String::new();
    let by_module = group_by_module();

    let _ = writeln!(out, "# Supported API Reference");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "This page is auto-generated from Perry's compile-time API manifest \
         (`perry-api-manifest::API_MANIFEST`). It is the source of truth for \
         what `perry compile` accepts; references to symbols not listed here \
         produce `R005 UnimplementedApi` (issue #463). Stubs (#464) are \
         flagged ⚠ — they link cleanly but no-op at runtime on the chosen target."
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "**Generated for Perry v{}.**", perry_version);
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Total: {} entries across {} modules.",
        API_MANIFEST.len(),
        by_module.len()
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "## Modules");
    let _ = writeln!(out);
    for module in by_module.keys() {
        let _ = writeln!(out, "- [`{}`](#{})", module, anchor(module));
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "---");
    let _ = writeln!(out);

    for (module, entries) in &by_module {
        let _ = writeln!(out, "## `{}`", module);
        let _ = writeln!(out);

        let methods: Vec<&ApiEntry> = entries
            .iter()
            .copied()
            .filter(|e| matches!(e.kind, ApiKind::Method { .. }))
            .collect();
        let properties: Vec<&ApiEntry> = entries
            .iter()
            .copied()
            .filter(|e| matches!(e.kind, ApiKind::Property))
            .collect();
        let classes: Vec<&ApiEntry> = entries
            .iter()
            .copied()
            .filter(|e| matches!(e.kind, ApiKind::Class))
            .collect();

        if !classes.is_empty() {
            let _ = writeln!(out, "### Classes");
            let _ = writeln!(out);
            for e in &classes {
                let _ = writeln!(out, "- `{}`{}", e.name, source_marker(e));
            }
            let _ = writeln!(out);
        }

        if !methods.is_empty() {
            let _ = writeln!(out, "### Methods");
            let _ = writeln!(out);
            for e in &methods {
                if let ApiKind::Method {
                    has_receiver,
                    class_filter,
                } = e.kind
                {
                    let receiver = if has_receiver { "instance" } else { "module" };
                    let cls = class_filter
                        .map(|c| format!(" *(class: `{}`)*", c))
                        .unwrap_or_default();
                    let _ = writeln!(
                        out,
                        "- `{}` — {}{}{}",
                        e.name,
                        receiver,
                        cls,
                        source_marker(e),
                    );
                }
            }
            let _ = writeln!(out);
        }

        if !properties.is_empty() {
            let _ = writeln!(out, "### Properties");
            let _ = writeln!(out);
            for e in &properties {
                let _ = writeln!(out, "- `{}`{}", e.name, source_marker(e));
            }
            let _ = writeln!(out);
        }
    }

    out
}

/// Render the manifest as a TypeScript declaration file (`.d.ts`).
/// Editors that load this get squiggles on unimplemented references
/// before `perry compile` runs. The declarations are intentionally
/// loose — `any` for parameters and return types — because the
/// manifest doesn't carry signature metadata yet (followup under #466
/// Phase 2 when wrapper authors declare argument types). The point is
/// existence, not type checking.
pub fn emit_dts(perry_version: &str) -> String {
    let mut out = String::new();
    let by_module = group_by_module();

    let _ = writeln!(
        out,
        "// Auto-generated from Perry's API manifest (#465). Do not edit by hand."
    );
    let _ = writeln!(out, "// Source: perry-api-manifest::API_MANIFEST");
    let _ = writeln!(out, "// Perry version: {}", perry_version);
    let _ = writeln!(
        out,
        "// Coverage: {} entries across {} modules",
        API_MANIFEST.len(),
        by_module.len()
    );
    let _ = writeln!(out);

    for (module, entries) in &by_module {
        let module_decl = module_declaration_name(module);
        let _ = writeln!(out, "declare module \"{}\" {{", module_decl);

        // Classes first — methods may reference them via class_filter.
        for e in entries.iter().filter(|e| matches!(e.kind, ApiKind::Class)) {
            let _ = writeln!(
                out,
                "  /** {}{} */",
                source_dts_tag(e),
                if e.stub {
                    " — stub (no-op at runtime)"
                } else {
                    ""
                }
            );
            let _ = writeln!(
                out,
                "  export class {} {{ [key: string]: any; }}",
                ts_ident(e.name)
            );
        }

        // Properties.
        for e in entries
            .iter()
            .filter(|e| matches!(e.kind, ApiKind::Property))
        {
            let _ = writeln!(
                out,
                "  /** {}{} */",
                source_dts_tag(e),
                if e.stub { " — stub" } else { "" }
            );
            let _ = writeln!(out, "  export const {}: any;", ts_ident(e.name));
        }

        // Module-level functions (has_receiver: false, no class_filter).
        // Instance methods (has_receiver: true) and class-filtered ones
        // hang off classes that aren't reflected in the manifest's
        // method entries — `[key: string]: any;` on the class above
        // makes their access compile, just without IDE squiggle help.
        // Followup under #466 will tighten this when signature data lands.
        let mut emitted_fn_names: std::collections::HashSet<&str> =
            std::collections::HashSet::new();
        for e in entries.iter().filter(|e| {
            matches!(
                e.kind,
                ApiKind::Method {
                    has_receiver: false,
                    class_filter: None,
                }
            )
        }) {
            // Same name can appear with multiple class_filter rows in
            // the dispatch table; the manifest collapses them but a
            // duplicate-emit guard keeps the output well-formed.
            if !emitted_fn_names.insert(e.name) {
                continue;
            }
            let _ = writeln!(
                out,
                "  /** {}{} */",
                source_dts_tag(e),
                if e.stub { " — stub" } else { "" }
            );
            // `default` is the npm convention for "the module is
            // callable" (e.g. `import sharp from 'sharp'`). TypeScript
            // expresses that as `export default function (...)`, with
            // no name on the function declaration.
            if e.name == "default" {
                let _ = writeln!(out, "  export default function (...args: any[]): any;");
            } else {
                let _ = writeln!(
                    out,
                    "  export function {}(...args: any[]): any;",
                    ts_ident(e.name)
                );
            }
        }

        let _ = writeln!(out, "}}");
        let _ = writeln!(out);
    }

    out
}

// -----------------------------------------------------------------------------
// helpers
// -----------------------------------------------------------------------------

fn group_by_module() -> BTreeMap<&'static str, Vec<&'static ApiEntry>> {
    let mut by_module: BTreeMap<&'static str, Vec<&'static ApiEntry>> = BTreeMap::new();
    for entry in API_MANIFEST {
        by_module.entry(entry.module).or_default().push(entry);
    }
    for entries in by_module.values_mut() {
        entries.sort_by_key(|e| (kind_order(&e.kind), e.name));
    }
    by_module
}

fn kind_order(kind: &ApiKind) -> u8 {
    match kind {
        ApiKind::Class => 0,
        ApiKind::Property => 1,
        ApiKind::Method { .. } => 2,
    }
}

fn source_marker(entry: &ApiEntry) -> String {
    let mut tag = match entry.source {
        ApiSource::Stdlib => String::new(),
        ApiSource::WellKnown => " *(well-known)*".to_string(),
        ApiSource::External => " *(external)*".to_string(),
        ApiSource::Intrinsic => " *(intrinsic)*".to_string(),
    };
    if entry.stub {
        tag.push_str(" ⚠ stub");
    }
    tag
}

fn source_dts_tag(entry: &ApiEntry) -> &'static str {
    match entry.source {
        ApiSource::Stdlib => "stdlib",
        ApiSource::WellKnown => "well-known",
        ApiSource::External => "external",
        ApiSource::Intrinsic => "intrinsic",
    }
}

/// Markdown anchor for a heading. mdbook lowercases and replaces
/// non-alphanum with `-`. Matches its slugifier closely enough for
/// the in-page TOC to land.
fn anchor(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

/// `mysql2/promise` becomes `mysql2/promise` — TS allows slash in
/// module specifiers. `perry/ui` → `perry/ui`. No transformation
/// needed today; kept as a hook for #466 Phase 2 if external manifests
/// ever need namespacing.
fn module_declaration_name(s: &str) -> &str {
    s
}

/// TypeScript identifiers can't start with a digit and forbid most
/// punctuation. Manifest names are already valid identifiers in
/// practice; this is just defensive in case a future entry adds one.
fn ts_ident(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '$' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out
        .chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false)
    {
        out.insert(0, '_');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_contains_every_module() {
        let md = emit_markdown("test");
        let modules: std::collections::HashSet<&'static str> =
            API_MANIFEST.iter().map(|e| e.module).collect();
        for m in &modules {
            // Modules render as `## `<name>``.
            assert!(
                md.contains(&format!("## `{}`", m)),
                "module heading missing: {}",
                m
            );
        }
    }

    #[test]
    fn dts_declares_every_module() {
        let dts = emit_dts("test");
        let modules: std::collections::HashSet<&'static str> =
            API_MANIFEST.iter().map(|e| e.module).collect();
        for m in &modules {
            assert!(
                dts.contains(&format!("declare module \"{}\"", m)),
                "module declaration missing: {}",
                m
            );
        }
    }

    #[test]
    fn dts_known_method_appears() {
        let dts = emit_dts("test");
        // crypto.randomUUID is a stable, no-receiver method — should
        // surface as `export function randomUUID(...)` in `declare
        // module "crypto"`.
        let crypto_block_start = dts.find("declare module \"crypto\"").expect("crypto block");
        let after = &dts[crypto_block_start..];
        let crypto_block_end = after.find("\n}\n").expect("block end");
        let crypto_block = &after[..crypto_block_end];
        assert!(
            crypto_block.contains("export function randomUUID"),
            "crypto.randomUUID missing from .d.ts"
        );
    }
}

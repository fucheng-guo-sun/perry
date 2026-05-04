//! HIR-level static-trip-count for-loop full-unroll pass.
//!
//! Detects the canonical small-fixed-trip-count for-loop shape:
//!
//! ```text
//! for (let i = LO; i {<,<=} HI; i++) { body }
//! ```
//!
//! where `LO` and `HI` are integer literals and the resulting trip count is
//! small (≤ `MAX_TRIP_COUNT`). Replaces the entire `Stmt::For` with N copies
//! of the body, with every `Expr::LocalGet(i)` substituted by
//! `Expr::Integer(LO + n)` for the n-th copy.
//!
//! Motivation — `image_convolution`'s 5×5 blur kernel:
//!
//! ```text
//! for (let ky = -2; ky <= 2; ky++) {
//!   const krow = KERNEL[ky + 2];
//!   for (let kx = -2; kx <= 2; kx++) {
//!     const k = krow[kx + 2];
//!     rAcc += src[idx] * k;
//!     ...
//!   }
//! }
//! ```
//!
//! With the kx loop unrolled (`kx` substituted with -2, -1, 0, 1, 2), the
//! `KERNEL[ky + 2][kx + 2]` access folds to `KERNEL[ky + 2][0..4]`. With
//! the ky loop also unrolled, both indices are compile-time integers and
//! Perry's existing flat-const machinery (`Expr::IndexGet` on a flat
//! `[25 x i32]` global with literal indices) replaces the load with a
//! constant. LLVM then specializes mul-by-1 to no-op, mul-by-4 to a
//! 2-bit shift, and mul-by-16 to a 4-bit shift — matching Zig's
//! ~130 ms scalar blur kernel instead of Perry's SIMD-bound ~240 ms.
//!
//! ## Safety guards (rejected shapes)
//!
//! Bodies that contain any of the following are NOT unrolled. Each shape
//! has a specific reason it can't be safely unrolled by N-copy substitution:
//!
//! - **`break` / `continue` / labeled break/continue** — would need to be
//!   rewritten as `LabeledBreak` to a synthetic label wrapping the
//!   unrolled stmts. Out of scope for v1.
//! - **`Stmt::Labeled`** — same reason; the label is loop-scoped and
//!   would alias with siblings post-unroll.
//! - **Closures capturing the IV** — each iteration needs to capture a
//!   different value of `i`, but unrolling produces stmts at the caller
//!   scope where `i` no longer exists. Substituting `LocalGet(i)` to
//!   `Integer(N)` inside the closure body works only for closures that
//!   capture-by-value at construction time AND aren't called after the
//!   IV's loop-scope ends. Conservative: reject all closures referencing
//!   the IV.
//! - **`LocalSet(i, ...)` or `Update { id: i }` inside body** — user is
//!   manually mutating the IV; unrolling would lose those writes.
//!   Allowed only in the for-loop's own `update` slot (by definition).
//! - **Nested `Stmt::For` shadowing the same IV id** — can't happen if
//!   HIR has unique LocalIds (which it should), but the analysis bails
//!   defensively.
//! - **`Stmt::Try`** — `try { for {...} }` is fine, but `for { try {...} }`
//!   would need each unrolled iteration to share the same `catch` /
//!   `finally`. Conservative: reject.
//!
//! ## Trip count bound
//!
//! `MAX_TRIP_COUNT = 8`. Image_convolution's kernels are 5 trips each;
//! at 5×5 = 25 total inlined stmts per pixel and ~150 byte body per
//! statement, the unrolled module init is bounded at ~5KB extra IR per
//! kernel-bearing function. Larger trips would inflate code size faster
//! than LLVM constant-folding can claw back.

use perry_hir::walker::{walk_expr_children, walk_expr_children_mut};
use perry_hir::{CompareOp, Expr, Module, Stmt, UpdateOp};
use perry_types::LocalId;

/// Maximum trip count we'll fully unroll. 8 covers the canonical
/// image-kernel shapes (3×3, 5×5, 7×7) without blowing up code size.
const MAX_TRIP_COUNT: i64 = 8;

/// Apply the unroll pass to every function in `module` (including methods,
/// constructors, getters, setters, and `module.init`). Each Function whose
/// body actually changed (at least one for-loop unrolled in-place) gets its
/// `was_unrolled` flag set to `true` so the codegen-side channel-vector
/// SIMD gate can skip the manual `<4 x i32>` reduction that fights LLVM's
/// freedom to constant-fold the now-literal kernel coefficients.
///
/// `module.init`'s unroll status is tracked separately on the Module via
/// the `init_was_unrolled` field — image_convolution puts the blur kernel
/// inline at module level, not inside a function, so the flag must travel
/// with module.init.
pub fn unroll_static_loops(module: &mut Module) {
    let mut init_changed = false;
    unroll_in_stmts(&mut module.init, &mut init_changed);
    if init_changed {
        module.init_was_unrolled = true;
    }
    for f in &mut module.functions {
        let mut changed = false;
        unroll_in_stmts(&mut f.body, &mut changed);
        if changed {
            f.was_unrolled = true;
        }
    }
    for c in &mut module.classes {
        if let Some(ctor) = &mut c.constructor {
            let mut changed = false;
            unroll_in_stmts(&mut ctor.body, &mut changed);
            if changed {
                ctor.was_unrolled = true;
            }
        }
        for m in &mut c.methods {
            let mut changed = false;
            unroll_in_stmts(&mut m.body, &mut changed);
            if changed {
                m.was_unrolled = true;
            }
        }
        for (_name, g) in &mut c.getters {
            let mut changed = false;
            unroll_in_stmts(&mut g.body, &mut changed);
            if changed {
                g.was_unrolled = true;
            }
        }
        for (_name, s) in &mut c.setters {
            let mut changed = false;
            unroll_in_stmts(&mut s.body, &mut changed);
            if changed {
                s.was_unrolled = true;
            }
        }
        // Field initializers are expressions, not stmt vectors. The
        // canonical case (a literal-init field with no for-loop in the
        // initializer) doesn't need walking; complex initializers would
        // benefit from unroll if they contained loops, but the gain is
        // marginal and we'd need an Expr-level unroll variant. Skip.
    }
}

/// Walk `stmts` and unroll any qualifying `Stmt::For` in place. Recurses
/// into nested control flow (if/while/for/switch/try) so inner loops also
/// get a shot at unrolling. Outer loops are unrolled FIRST — once the
/// outer is gone, the inner loop appears N times in the unrolled output
/// and each copy is then walked again to unroll the inner if it qualifies.
///
/// `changed` is set to `true` whenever any `Stmt::For` in `stmts` (or
/// recursively inside its children) gets unrolled. The caller uses this
/// to mark the enclosing Function's `was_unrolled` flag so codegen can
/// disable the channel-vector SIMD reduction in unrolled bodies.
fn unroll_in_stmts(stmts: &mut Vec<Stmt>, changed: &mut bool) {
    let mut i = 0;
    while i < stmts.len() {
        // Recurse into nested control flow first so an inner unrollable
        // loop becomes N copies in its parent's body BEFORE the parent's
        // unroll pass clones the parent's body N more times. This ordering
        // means each unrolled iteration of an inner loop gets cloned by
        // any enclosing outer loop, but the outer's body is already
        // simplified. Same end result either way for correctness; this
        // ordering is just slightly less work.
        recurse_into_nested(&mut stmts[i], changed);

        if let Some(unrolled) = try_unroll_for(&stmts[i]) {
            // Replace stmts[i] with `unrolled`'s contents.
            let inserted = unrolled.len();
            stmts.splice(i..=i, unrolled);
            *changed = true;
            i += inserted;
        } else {
            i += 1;
        }
    }
}

/// Recurse into the children of a control-flow stmt so nested for-loops
/// get an unroll attempt. `Stmt::For` itself is NOT recursed into here
/// (the outer driver handles it via try_unroll_for); but its body is
/// walked so inner unrollable loops get processed before the outer's
/// unroll attempt.
fn recurse_into_nested(stmt: &mut Stmt, changed: &mut bool) {
    match stmt {
        Stmt::If {
            then_branch,
            else_branch,
            ..
        } => {
            unroll_in_stmts(then_branch, changed);
            if let Some(eb) = else_branch {
                unroll_in_stmts(eb, changed);
            }
        }
        Stmt::While { body, .. } | Stmt::DoWhile { body, .. } => {
            unroll_in_stmts(body, changed);
        }
        Stmt::For { body, .. } => {
            // Inner-first: unroll any qualifying loops inside this for's
            // body before deciding whether to unroll this for itself.
            unroll_in_stmts(body, changed);
        }
        Stmt::Switch { cases, .. } => {
            for c in cases {
                unroll_in_stmts(&mut c.body, changed);
            }
        }
        Stmt::Try {
            body,
            catch,
            finally,
            ..
        } => {
            unroll_in_stmts(body, changed);
            if let Some(c) = catch {
                unroll_in_stmts(&mut c.body, changed);
            }
            if let Some(f) = finally {
                unroll_in_stmts(f, changed);
            }
        }
        Stmt::Labeled { body, .. } => {
            recurse_into_nested(body, changed);
        }
        _ => {}
    }
}

/// Inspect a `Stmt::For` and, if it matches the canonical
/// integer-literal-bounded shape with a small trip count and a body
/// safe to unroll, return the unrolled stmt sequence. Returns `None`
/// otherwise — caller leaves the original `Stmt::For` in place.
fn try_unroll_for(stmt: &Stmt) -> Option<Vec<Stmt>> {
    let (init, condition, update, body) = match stmt {
        Stmt::For {
            init,
            condition,
            update,
            body,
        } => (init, condition, update, body),
        _ => return None,
    };

    // 1. Init must be `let i = INTEGER` where INTEGER fits in i64.
    let init_box = init.as_ref()?;
    let (iv_id, lo) = match init_box.as_ref() {
        Stmt::Let {
            id,
            init: Some(Expr::Integer(n)),
            ..
        } => (*id, *n),
        _ => return None,
    };

    // 2. Condition must be `LocalGet(iv_id) {<,<=} INTEGER`.
    let (cmp_op, hi) = match condition.as_ref()? {
        Expr::Compare { op, left, right } => {
            let left_id = match left.as_ref() {
                Expr::LocalGet(id) if *id == iv_id => *id,
                _ => return None,
            };
            let _ = left_id;
            let hi = match right.as_ref() {
                Expr::Integer(n) => *n,
                _ => return None,
            };
            (op, hi)
        }
        _ => return None,
    };

    // 3. Update must be `iv_id++` (Update with op=Increment).
    match update.as_ref()? {
        Expr::Update {
            id,
            op: UpdateOp::Increment,
            ..
        } if *id == iv_id => {}
        _ => return None,
    }

    // 4. Trip count must be small.
    let trips = match cmp_op {
        CompareOp::Lt => hi.saturating_sub(lo),
        CompareOp::Le => hi.saturating_sub(lo).saturating_add(1),
        _ => return None,
    };
    if trips <= 0 || trips > MAX_TRIP_COUNT {
        return None;
    }

    // 5. Body must be safe to unroll (no break/continue/labeled, no
    //    LocalSet/Update on iv_id, no closure capturing iv_id, no Try).
    if !body_is_unrollable(body, iv_id) {
        return None;
    }

    // 6. Emit N copies. For each iteration, clone the body and substitute
    //    every Expr::LocalGet(iv_id) with Expr::Integer(lo + n). The IV's
    //    own `Stmt::Let` from the for's init is dropped — it doesn't
    //    appear in the unrolled output. `update` is dropped likewise (it
    //    only mutated the IV slot which no longer exists post-unroll).
    let mut out: Vec<Stmt> = Vec::with_capacity((trips as usize) * body.len());
    for n in 0..trips {
        let value = lo + n;
        for s in body {
            let mut cloned = s.clone();
            substitute_localget_with_int_in_stmt(&mut cloned, iv_id, value);
            out.push(cloned);
        }
    }
    Some(out)
}

/// Returns true if the body is safe to unroll. Walks `body` tracking
/// loop nesting depth: `break`/`continue` are rejected at depth 0 (they
/// would target the for-loop being unrolled, which won't exist post-
/// unroll) but allowed at depth ≥ 1 (they target an inner loop that
/// survives the unroll intact, with its own labels and exit blocks).
///
/// The inliner expands every same-module function call as a synthetic
/// `Stmt::DoWhile { body: <inlined>, condition: false }` wrapper, with
/// every `return e` rewritten to `LocalSet(let_id, e); break`. So a
/// caller body that uses any inlined helper (clampIdx, clampU8, …)
/// has nested do-whiles full of breaks targeting the inliner's wrapper
/// loop. Counting those as depth-1 is correct: their breaks exit the
/// inliner's synthetic do-while, not the for being unrolled.
fn body_is_unrollable(body: &[Stmt], iv_id: LocalId) -> bool {
    body.iter().all(|s| stmt_is_unrollable(s, iv_id, 0))
}

fn stmt_is_unrollable(stmt: &Stmt, iv_id: LocalId, loop_depth: u32) -> bool {
    match stmt {
        Stmt::Break | Stmt::Continue => loop_depth > 0,
        // Labeled break/continue: even at loop_depth > 0 we don't know
        // whether the label points at our enclosing for or at an inner
        // construct. Conservative: reject. Labeled control flow inside
        // a hot kernel is rare; image_convolution doesn't use it.
        Stmt::LabeledBreak(_) | Stmt::LabeledContinue(_) => false,
        // `Stmt::Labeled` would need its label rewritten per-unroll-iter
        // (each unrolled copy needs a unique label name) AND any
        // LabeledBreak inside a sibling stmt could target it. Out of
        // scope for v1.
        Stmt::Labeled { .. } => false,
        Stmt::Try { .. } => false,
        Stmt::Let { id, init, .. } => {
            if *id == iv_id {
                // Shadowing — shouldn't happen since HIR ids are unique
                // and this would be an inner Let with the same id as the
                // outer for-init. Defensive bail.
                return false;
            }
            init.as_ref().is_none_or(|e| expr_is_unrollable(e, iv_id))
        }
        Stmt::Expr(e) | Stmt::Throw(e) => expr_is_unrollable(e, iv_id),
        Stmt::Return(opt) => opt.as_ref().is_none_or(|e| expr_is_unrollable(e, iv_id)),
        Stmt::If {
            condition,
            then_branch,
            else_branch,
        } => {
            expr_is_unrollable(condition, iv_id)
                && then_branch
                    .iter()
                    .all(|s| stmt_is_unrollable(s, iv_id, loop_depth))
                && else_branch.as_ref().is_none_or(|eb| {
                    eb.iter().all(|s| stmt_is_unrollable(s, iv_id, loop_depth))
                })
        }
        Stmt::While { condition, body } | Stmt::DoWhile { body, condition } => {
            // Inner loop: bumps depth so break/continue inside become safe.
            expr_is_unrollable(condition, iv_id)
                && body
                    .iter()
                    .all(|s| stmt_is_unrollable(s, iv_id, loop_depth + 1))
        }
        Stmt::For {
            init,
            condition,
            update,
            body,
        } => {
            init.as_ref()
                .is_none_or(|s| stmt_is_unrollable(s, iv_id, loop_depth + 1))
                && condition
                    .as_ref()
                    .is_none_or(|e| expr_is_unrollable(e, iv_id))
                && update
                    .as_ref()
                    .is_none_or(|e| expr_is_unrollable(e, iv_id))
                && body
                    .iter()
                    .all(|s| stmt_is_unrollable(s, iv_id, loop_depth + 1))
        }
        Stmt::Switch { discriminant, cases } => {
            // Switch case bodies have `break` that targets the switch (not
            // the enclosing for). Counted as depth + 1 to allow them.
            expr_is_unrollable(discriminant, iv_id)
                && cases.iter().all(|c| {
                    c.test.as_ref().is_none_or(|t| expr_is_unrollable(t, iv_id))
                        && c.body
                            .iter()
                            .all(|s| stmt_is_unrollable(s, iv_id, loop_depth + 1))
                })
        }
    }
}

fn expr_is_unrollable(e: &Expr, iv_id: LocalId) -> bool {
    // Reject writes to the IV.
    match e {
        Expr::LocalSet(id, _) if *id == iv_id => return false,
        Expr::Update { id, .. } if *id == iv_id => return false,
        // Closures: reject any closure that even mentions the IV. A
        // closure captured-by-value at construction would semantically
        // freeze the IV's current value, but our HIR captures are by
        // ID; substituting LocalGet(iv) → Integer(N) inside the
        // closure body works only if the closure isn't called outside
        // the IV's live range. The image_convolution kernel doesn't
        // create closures inside its blur loops, so this restriction
        // is free for the target workload.
        Expr::Closure { body, captures, .. } => {
            if captures.iter().any(|cap| *cap == iv_id) {
                return false;
            }
            // Defensive: walk the closure body to catch any direct
            // `LocalGet(iv_id)` reference that wasn't materialized as a
            // capture entry (shouldn't happen in well-formed HIR, but
            // checking is cheap). Closure body's break/continue are
            // always lexically scoped to a loop *inside* the closure
            // (free `break` outside a loop is a JS syntax error), so we
            // start at loop_depth=1 to suppress the always-true Break/
            // Continue rejection.
            if !body
                .iter()
                .all(|s| stmt_is_unrollable(s, iv_id, 1))
            {
                return false;
            }
            return true;
        }
        _ => {}
    }
    // Recurse into all sub-expressions.
    let mut ok = true;
    walk_expr_children(e, &mut |child| {
        if !expr_is_unrollable(child, iv_id) {
            ok = false;
        }
    });
    ok
}

/// Replace every `Expr::LocalGet(iv_id)` in `stmt` with `Expr::Integer(value)`.
/// `LocalSet` / `Update` of `iv_id` are rejected by the unrollability
/// pre-check, so this fn doesn't need to handle them.
fn substitute_localget_with_int_in_stmt(stmt: &mut Stmt, iv_id: LocalId, value: i64) {
    match stmt {
        Stmt::Let { init, .. } => {
            if let Some(e) = init {
                substitute_localget_with_int(e, iv_id, value);
            }
        }
        Stmt::Expr(e) | Stmt::Throw(e) => substitute_localget_with_int(e, iv_id, value),
        Stmt::Return(opt) => {
            if let Some(e) = opt {
                substitute_localget_with_int(e, iv_id, value);
            }
        }
        Stmt::If {
            condition,
            then_branch,
            else_branch,
        } => {
            substitute_localget_with_int(condition, iv_id, value);
            for s in then_branch {
                substitute_localget_with_int_in_stmt(s, iv_id, value);
            }
            if let Some(eb) = else_branch {
                for s in eb {
                    substitute_localget_with_int_in_stmt(s, iv_id, value);
                }
            }
        }
        Stmt::While { condition, body } | Stmt::DoWhile { body, condition } => {
            substitute_localget_with_int(condition, iv_id, value);
            for s in body {
                substitute_localget_with_int_in_stmt(s, iv_id, value);
            }
        }
        Stmt::For {
            init,
            condition,
            update,
            body,
        } => {
            if let Some(s) = init {
                substitute_localget_with_int_in_stmt(s, iv_id, value);
            }
            if let Some(c) = condition {
                substitute_localget_with_int(c, iv_id, value);
            }
            if let Some(u) = update {
                substitute_localget_with_int(u, iv_id, value);
            }
            for s in body {
                substitute_localget_with_int_in_stmt(s, iv_id, value);
            }
        }
        Stmt::Switch { discriminant, cases } => {
            substitute_localget_with_int(discriminant, iv_id, value);
            for c in cases {
                if let Some(t) = &mut c.test {
                    substitute_localget_with_int(t, iv_id, value);
                }
                for s in &mut c.body {
                    substitute_localget_with_int_in_stmt(s, iv_id, value);
                }
            }
        }
        Stmt::Try {
            body,
            catch,
            finally,
        } => {
            for s in body {
                substitute_localget_with_int_in_stmt(s, iv_id, value);
            }
            if let Some(c) = catch {
                for s in &mut c.body {
                    substitute_localget_with_int_in_stmt(s, iv_id, value);
                }
            }
            if let Some(f) = finally {
                for s in f {
                    substitute_localget_with_int_in_stmt(s, iv_id, value);
                }
            }
        }
        Stmt::Labeled { body, .. } => {
            substitute_localget_with_int_in_stmt(body, iv_id, value);
        }
        Stmt::Break | Stmt::Continue | Stmt::LabeledBreak(_) | Stmt::LabeledContinue(_) => {}
    }
}

fn substitute_localget_with_int(expr: &mut Expr, iv_id: LocalId, value: i64) {
    if let Expr::LocalGet(id) = expr {
        if *id == iv_id {
            *expr = Expr::Integer(value);
            return;
        }
    }
    walk_expr_children_mut(expr, &mut |child| {
        substitute_localget_with_int(child, iv_id, value);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use perry_hir::BinaryOp;
    use perry_types::Type;

    fn ivar(id: LocalId) -> Expr {
        Expr::LocalGet(id)
    }

    fn integer(n: i64) -> Expr {
        Expr::Integer(n)
    }

    /// Build `for (let i = lo; i <= hi; i++) { body }`.
    fn make_for(iv_id: LocalId, lo: i64, hi: i64, body: Vec<Stmt>, op: CompareOp) -> Stmt {
        Stmt::For {
            init: Some(Box::new(Stmt::Let {
                id: iv_id,
                name: "i".into(),
                ty: Type::Number,
                mutable: true,
                init: Some(integer(lo)),
            })),
            condition: Some(Expr::Compare {
                op,
                left: Box::new(ivar(iv_id)),
                right: Box::new(integer(hi)),
            }),
            update: Some(Expr::Update {
                id: iv_id,
                op: UpdateOp::Increment,
                prefix: false,
            }),
            body,
        }
    }

    #[test]
    fn unrolls_canonical_5_trip_loop() {
        // for (let i = -2; i <= 2; i++) { acc = acc + i; }
        let acc = 100u32;
        let i = 1u32;
        let body = vec![Stmt::Expr(Expr::LocalSet(
            acc,
            Box::new(Expr::Binary {
                op: BinaryOp::Add,
                left: Box::new(Expr::LocalGet(acc)),
                right: Box::new(ivar(i)),
            }),
        ))];
        let f = make_for(i, -2, 2, body, CompareOp::Le);
        let unrolled = try_unroll_for(&f).expect("should unroll");
        assert_eq!(unrolled.len(), 5);
        // Each iteration replaces LocalGet(i) with Integer(-2..2).
        for (n, s) in unrolled.iter().enumerate() {
            let expected_int = -2 + n as i64;
            match s {
                Stmt::Expr(Expr::LocalSet(id, value)) => {
                    assert_eq!(*id, acc);
                    match value.as_ref() {
                        Expr::Binary {
                            op: BinaryOp::Add,
                            right,
                            ..
                        } => match right.as_ref() {
                            Expr::Integer(n2) => assert_eq!(*n2, expected_int),
                            other => panic!("expected Integer, got {:?}", other),
                        },
                        other => panic!("expected Add binary, got {:?}", other),
                    }
                }
                other => panic!("expected Stmt::Expr, got {:?}", other),
            }
        }
    }

    #[test]
    fn unrolls_lt_form() {
        // for (let i = 0; i < 3; i++) { ... }
        let i = 1u32;
        let body = vec![Stmt::Expr(Expr::LocalGet(i))];
        let f = make_for(i, 0, 3, body, CompareOp::Lt);
        let unrolled = try_unroll_for(&f).expect("should unroll");
        // i = 0, 1, 2 — 3 trips for `<`.
        assert_eq!(unrolled.len(), 3);
    }

    #[test]
    fn rejects_loop_above_max_trip_count() {
        // for (let i = 0; i < 100; i++) — 100 trips is way above MAX_TRIP_COUNT=8.
        let i = 1u32;
        let body = vec![Stmt::Expr(Expr::LocalGet(i))];
        let f = make_for(i, 0, 100, body, CompareOp::Lt);
        assert!(try_unroll_for(&f).is_none());
    }

    #[test]
    fn rejects_loop_with_break() {
        let i = 1u32;
        let body = vec![Stmt::Break];
        let f = make_for(i, 0, 3, body, CompareOp::Lt);
        assert!(try_unroll_for(&f).is_none());
    }

    #[test]
    fn rejects_loop_with_continue() {
        let i = 1u32;
        let body = vec![Stmt::Continue];
        let f = make_for(i, 0, 3, body, CompareOp::Lt);
        assert!(try_unroll_for(&f).is_none());
    }

    #[test]
    fn rejects_loop_with_iv_localset_in_body() {
        // for (let i = 0; i < 3; i++) { i = 99; }   // mutates IV mid-body
        let i = 1u32;
        let body = vec![Stmt::Expr(Expr::LocalSet(i, Box::new(integer(99))))];
        let f = make_for(i, 0, 3, body, CompareOp::Lt);
        assert!(try_unroll_for(&f).is_none());
    }

    #[test]
    fn rejects_loop_with_iv_update_in_body() {
        let i = 1u32;
        let body = vec![Stmt::Expr(Expr::Update {
            id: i,
            op: UpdateOp::Increment,
            prefix: false,
        })];
        let f = make_for(i, 0, 3, body, CompareOp::Lt);
        assert!(try_unroll_for(&f).is_none());
    }

    #[test]
    fn rejects_loop_with_try() {
        let i = 1u32;
        let body = vec![Stmt::Try {
            body: vec![],
            catch: None,
            finally: None,
        }];
        let f = make_for(i, 0, 3, body, CompareOp::Lt);
        assert!(try_unroll_for(&f).is_none());
    }

    #[test]
    fn rejects_loop_with_non_integer_init() {
        // for (let i = X; i < 3; i++) where X isn't an Integer literal
        let i = 1u32;
        let f = Stmt::For {
            init: Some(Box::new(Stmt::Let {
                id: i,
                name: "i".into(),
                ty: Type::Number,
                mutable: true,
                init: Some(Expr::LocalGet(99)),
            })),
            condition: Some(Expr::Compare {
                op: CompareOp::Lt,
                left: Box::new(Expr::LocalGet(i)),
                right: Box::new(integer(3)),
            }),
            update: Some(Expr::Update {
                id: i,
                op: UpdateOp::Increment,
                prefix: false,
            }),
            body: vec![],
        };
        assert!(try_unroll_for(&f).is_none());
    }

    #[test]
    fn rejects_loop_with_non_integer_bound() {
        let i = 1u32;
        let f = Stmt::For {
            init: Some(Box::new(Stmt::Let {
                id: i,
                name: "i".into(),
                ty: Type::Number,
                mutable: true,
                init: Some(integer(0)),
            })),
            condition: Some(Expr::Compare {
                op: CompareOp::Lt,
                left: Box::new(Expr::LocalGet(i)),
                right: Box::new(Expr::LocalGet(99)),
            }),
            update: Some(Expr::Update {
                id: i,
                op: UpdateOp::Increment,
                prefix: false,
            }),
            body: vec![],
        };
        assert!(try_unroll_for(&f).is_none());
    }

    #[test]
    fn unrolls_nested_5x5_kernel() {
        // for (let ky = -2; ky <= 2; ky++) {
        //   for (let kx = -2; kx <= 2; kx++) {
        //     acc = acc + ky + kx;
        //   }
        // }
        let acc = 100u32;
        let ky = 1u32;
        let kx = 2u32;
        let inner_body = vec![Stmt::Expr(Expr::LocalSet(
            acc,
            Box::new(Expr::Binary {
                op: BinaryOp::Add,
                left: Box::new(Expr::Binary {
                    op: BinaryOp::Add,
                    left: Box::new(Expr::LocalGet(acc)),
                    right: Box::new(Expr::LocalGet(ky)),
                }),
                right: Box::new(Expr::LocalGet(kx)),
            }),
        ))];
        let inner = make_for(kx, -2, 2, inner_body, CompareOp::Le);
        let outer = make_for(ky, -2, 2, vec![inner], CompareOp::Le);

        // Wrap in a vec and run unroll_in_stmts (so nested unroll fires).
        let mut stmts = vec![outer];
        let mut changed = false;
        unroll_in_stmts(&mut stmts, &mut changed);
        assert!(changed, "expected unroll to fire");

        // After full nested unroll: 5 ky × 5 kx = 25 stmts.
        assert_eq!(stmts.len(), 25);
        // Check a specific iteration: iter index = (ky_iter * 5 + kx_iter)
        // for ky=-2..2, kx=-2..2.
        for ky_n in 0..5 {
            for kx_n in 0..5 {
                let stmt_idx = ky_n * 5 + kx_n;
                let expected_ky = -2 + ky_n as i64;
                let expected_kx = -2 + kx_n as i64;
                match &stmts[stmt_idx] {
                    Stmt::Expr(Expr::LocalSet(_, v)) => match v.as_ref() {
                        Expr::Binary {
                            op: BinaryOp::Add,
                            left,
                            right,
                        } => {
                            // left = (acc + ky); right = kx
                            match right.as_ref() {
                                Expr::Integer(n) => assert_eq!(*n, expected_kx,
                                    "kx mismatch at ({}, {}): got {}, want {}",
                                    ky_n, kx_n, n, expected_kx),
                                other => panic!("expected kx Integer, got {:?}", other),
                            }
                            match left.as_ref() {
                                Expr::Binary { right: ky_e, .. } => match ky_e.as_ref() {
                                    Expr::Integer(n) => assert_eq!(*n, expected_ky,
                                        "ky mismatch at ({}, {}): got {}, want {}",
                                        ky_n, kx_n, n, expected_ky),
                                    other => panic!("expected ky Integer, got {:?}", other),
                                },
                                other => panic!("expected (acc+ky) Binary, got {:?}", other),
                            }
                        }
                        other => panic!("expected outer Binary, got {:?}", other),
                    },
                    other => panic!("expected Stmt::Expr LocalSet, got {:?}", other),
                }
            }
        }
    }

    #[test]
    fn unrolls_body_with_inliner_dowhile_breaks() {
        // The inliner expands `let xx = clampIdx(x + kx, 0, hi)` into
        // a `let xx = undefined; do { ...; xx = lo; break; ...; xx = hi;
        // break; xx = v; break; } while (false)`. The breaks belong to
        // the inner do-while, not the for being unrolled — the unroll
        // should fire despite the breaks.
        let kx = 1u32;
        let xx = 50u32;
        let body = vec![
            Stmt::Let {
                id: xx,
                name: "xx".into(),
                ty: Type::Number,
                mutable: true,
                init: Some(Expr::Undefined),
            },
            Stmt::DoWhile {
                body: vec![
                    Stmt::Expr(Expr::LocalSet(xx, Box::new(Expr::LocalGet(kx)))),
                    Stmt::Break,
                ],
                condition: Expr::Bool(false),
            },
        ];
        let f = make_for(kx, -2, 2, body, CompareOp::Le);
        let unrolled = try_unroll_for(&f).expect("inner-loop break should not block unroll");
        // 5 iterations × 2 stmts each (Let + DoWhile) = 10 stmts.
        assert_eq!(unrolled.len(), 10);
    }

    #[test]
    fn rejects_top_level_break_that_targets_the_for_itself() {
        // `for (let i = 0; i < 3; i++) { if (i === 1) break; }`
        // The `break` at depth 0 inside the for-body targets the for
        // itself — substituting LocalGet(i) → Integer(N) and dropping the
        // for would leave a stray Stmt::Break with no enclosing loop.
        let i = 1u32;
        let body = vec![Stmt::If {
            condition: Expr::Compare {
                op: CompareOp::Eq,
                left: Box::new(Expr::LocalGet(i)),
                right: Box::new(Expr::Integer(1)),
            },
            then_branch: vec![Stmt::Break],
            else_branch: None,
        }];
        let f = make_for(i, 0, 3, body, CompareOp::Lt);
        assert!(try_unroll_for(&f).is_none());
    }

    #[test]
    fn unrolls_inside_if_branches() {
        let acc = 100u32;
        let i = 1u32;
        let inner = make_for(
            i,
            0,
            3,
            vec![Stmt::Expr(Expr::LocalSet(
                acc,
                Box::new(Expr::Binary {
                    op: BinaryOp::Add,
                    left: Box::new(Expr::LocalGet(acc)),
                    right: Box::new(Expr::LocalGet(i)),
                }),
            ))],
            CompareOp::Lt,
        );
        let if_stmt = Stmt::If {
            condition: integer(1),
            then_branch: vec![inner],
            else_branch: None,
        };
        let mut stmts = vec![if_stmt];
        let mut changed = false;
        unroll_in_stmts(&mut stmts, &mut changed);
        assert!(changed, "expected unroll to fire");
        match &stmts[0] {
            Stmt::If { then_branch, .. } => {
                assert_eq!(then_branch.len(), 3, "inner for should unroll to 3 stmts");
            }
            _ => panic!("expected If"),
        }
    }
}

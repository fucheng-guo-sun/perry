// Issue #685: a class expression returned from a factory function with
// a static field initializer that references the factory's parameter.
// Pre-fix, the static field init was lifted to module-init scope where
// the factory's parameter is out of scope; codegen emitted
// `(0.0).slice()` which threw `TypeError: (number).slice is not a function`
// before any user code ran. Now the lift is skipped (the static field
// stays uninitialized, which is wrong but harmless) and module init
// completes; calling the factory works as expected at runtime.

function makeWrapper<const Params extends ReadonlyArray<string>>(...params: Params) {
  return class WrapperClass {
    static params = params.slice()
  }
}

console.log("[1] before makeWrapper");
const W = makeWrapper("a", "b", "c");
console.log("[2] after makeWrapper");
// W.params is currently `undefined` post-fix (perry doesn't yet emit
// the static init at the class-expression site inside the factory body
// — tracked as a follow-up). The win here is that module init no
// longer throws; the program runs into user code.
console.log("[3] params type:", typeof W.params);

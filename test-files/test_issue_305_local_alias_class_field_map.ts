// Closes #305: `const m = this.map` local alias of a class-field Map<K,V>
// dropped the Generic{base="Map"} type because infer_type_from_expr's Member
// arm hit the catch-all _ => Type::Any. Result: `m.set(k, v)` and
// `for (const [k,v] of m)` both fell off the Map fast path — set wrote into
// the wrong slot (key showed as 0) and for-of ran 0 iterations. v0.5.388's
// #302 fix added the same registry lookup for the for-of source resolver but
// not for the Let-RHS type inference. Same one-line gap, different consumer.

class Example {
  private map: Map<number, string> = new Map();

  runViaLocal(key: number): void {
    const m = this.map;
    m.set(key, "hello");

    const keys = Array.from(m.keys());
    let iterCount = 0;
    for (const [k, v] of m) {
      iterCount++;
    }

    console.log(`size: ${m.size}`);
    console.log(`keys: ${JSON.stringify(keys)}`);
    console.log(`iterCount: ${iterCount}`);
    this.map.clear();
  }

  runViaDirect(key: number): void {
    this.map.set(key, "hello");

    const keys = Array.from(this.map.keys());
    let iterCount = 0;
    for (const [k, v] of this.map) {
      iterCount++;
    }

    console.log(`size: ${this.map.size}`);
    console.log(`keys: ${JSON.stringify(keys)}`);
    console.log(`iterCount: ${iterCount}`);
    this.map.clear();
  }
}

class WithSet {
  private items: Set<string> = new Set();

  fillAndIter(): void {
    const s = this.items;
    s.add("alpha");
    s.add("beta");
    let count = 0;
    for (const v of s) {
      count++;
    }
    console.log(`set size: ${s.size}, iter: ${count}`);
  }
}

class WithArray {
  private xs: number[] = [];

  fillAndIter(): void {
    const a = this.xs;
    a.push(1);
    a.push(2);
    a.push(3);
    let sum = 0;
    for (const x of a) {
      sum += x;
    }
    console.log(`array length: ${a.length}, sum: ${sum}`);
  }
}

const ex = new Example();
ex.runViaLocal(1025);
ex.runViaDirect(2048);
const ws = new WithSet();
ws.fillAndIter();
const wa = new WithArray();
wa.fillAndIter();

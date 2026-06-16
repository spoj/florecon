# Strategy as a general State+Writer monad, and where conservation lives

> Status: **exploration / parked.** Captures a design thread. Not implemented.
> Revisit before touching `Strategy`, `Resolution`, `Group`, or `split`.

## 1. What `Strategy` already is

Strip the names and `Strategy<E>` is a specialized **State + Writer** monad —
the same arrow as a parser combinator `str -> (out, str)`:

```
Strategy<E> :   Vec<Entry<E>>   ->   ( Vec<Group> ,  Vec<Entry<E>> )
                    state in           writer out      state out
```

- **State channel** = the residual bag (threaded, consumed; the `str` remainder).
- **Writer channel** = `Vec<Group>` (accumulated, monoidal; `seq`/`when`/
  `fixed_point` just `extend` it).

The parser-style polymorphism (`out` can be anything) lives in the **writer
channel**, and today it is nailed shut to `Vec<Group>`.

Note: florecon's `seq` is *monoidal accumulation* (concat groups), not the
*product* `seq` of parsers (which builds `(A, B)` via a data dependency). So
florecon is the `many`/`alt` fragment — the writer is a **monoid you keep
folding into**, not a product builder. Consequence: `seq` is **homogeneous**
(all steps share the writer type); to mix types you `map` into a common sum
type first.

## 2. Two axes of generalization (don't conflate them)

The justification for polymorphic yield is **not** post-hoc reporting
(gross/net/compression/netting-efficiency all fold out of `Vec<Group>` after the
fact — they justify nothing). The only real win:

> A **leaf** wants to accumulate something domain-specific that the `Group`
> shape can't hold, and the user adds it **without editing the core type.**

That is **writer** polymorphism. Separately:

> **Partial-lot vs whole-lot is a *state* question, not a writer question.**

A whole-lot residual is `Vec<Entry<E>>` (whole rows). A partial-lot residual must
carry *remainders* (a 100-share lot half-consumed leaves a 50-share remnant).
That is a different `S`. Polymorphic *yield alone buys nothing for partial lots.*

## 3. The general shape: `S -> (O, S)` with `O: Monoid`

```rust
trait Proc<S, O: Monoid> { fn run(&self, s: S) -> (O, S); }
//  parser:  S = str,  O = AST
```

Three cases become instantiations of one monad; combinators written **once**:

| case        | `S` (state/residual)          | `O` (writer)                | conserves |
|-------------|-------------------------------|-----------------------------|-----------|
| whole-lot   | `Vec<Entry<E>>`               | `Vec<Group>` (member `Id`)  | identity  |
| partial-lot | `Vec<Lot<E>>` (id + qty)      | `Vec<Alloc>` (member `(Id,Qty)`) | quantity |
| allocate    | `Measure` (unallocated mass)  | `Measure` (allocated)       | value     |

`allocate` genuinely fits: it consumes coarse mass (state shrinks toward `ANY`)
and emits fine cells (writer); the README's "residual parks on `ANY`, in-cube"
*is* the threaded state remainder. The duality stops being rhetorical: one monad,
three conservation laws.

`Group` becomes polymorphic in **two** positions: membership and annotation —
`Group<M, A>`, `M = Id` (whole) or `M = (Id, Qty)` (partial). That avoids
bloating the shape.

### Why this fights bloat
The combinators (`seq`, `when`, `partition_by`, `windowed`, `fixed_point`,
`restart`, `explain`) are the bulk of the lib and are state-and-writer-generic —
written once, reused across all three. No `partial_seq`/`whole_seq` duplication.

### Honest costs
1. **`S` needs capability traits**, not just `Monoid`: `when`/`partition_by` must
   split/route state → `trait Bag: Monoid + Split + Filter + Fingerprint`
   (fingerprint for `fixed_point` convergence). `Measure` already has
   `slice`/`partition`/`select`, so it qualifies.
2. **Chaining forces `S_in = S_out`** (step 2 runs on step 1's residual), so it's
   the 2-channel monad `(S, O)`, not a free `In -> (Out, Rest)`. `E` lives inside
   `S`. Don't over-generalize past two channels.
3. **You lose the single crisp promise.** Conservation becomes *per-instance*
   (identity/quantity/value); the frame only guarantees *threading*. Some leaves
   (`exact_1to1` sign-pairing) are inherently whole-entry and stay whole-lot-only.
   The frame generalizes; not every leaf does.

### Recommended spike (before any refactor)
1. `trait Bag: Monoid + Split + Fingerprint`.
2. Re-express **`seq` + `fixed_point`** generically over `S: Bag, O: Monoid`.
3. Instantiate whole-lot (existing types must compile unchanged behind defaults)
   **and** a toy partial-lot `S = Vec<Lot>` with one leaf.
If `seq`/`fixed_point` come out clean and the partial-lot toy threads remainders
without special-casing, the unification carries the combinator weight. If the
capability traits sprout per-domain methods, it's two libraries in a trenchcoat —
stop.

## 4. Where conservation lives today

No conservation *type*. `Resolution<E>` is two plain vecs. The promise lives in
**`split()`**, the one function every leaf funnels through:

```rust
let in_group: HashSet<Id> = matched.iter().flatten().copied().collect();
let groups   = matched.into_iter().map(|ids| Group::new(ids, origin)).collect();
let residual = bag.into_iter().filter(|e| !in_group.contains(&e.id)).collect();
```

`residual = bag − matched` (set complement) is the **whole of conservation**. It
guarantees *no-loss* (every unclaimed entry survives, once). It does **not**
guarantee no-duplication or no-invention:

- `in_group` is a `HashSet` (dedups). A leaf that puts id `5` in two clusters →
  **both groups keep `5`** (groups built from raw `matched`), residual drops it
  once → `5` lives in two groups, silently. `split` never checks.
- `matched` naming an id not in `bag` → group with a phantom member; silent.

So conservation is **asserted, not proven** — same stance the README states for
alloc ("a property you assert, `a.total()`, not one the compiler enforces").
Combinators preserve it by threading residual in one shape (`matching ⊎ rest`).

## 5. The general expression: a measure homomorphism

Pick a measure `μ: S → Monoid`. Conservation is:

```
μ(O) ⊕ μ(S_out) = μ(S_in)        for every run()
```

— the step **moves** μ-mass from state into the writer; never creates/destroys.
`μ = count` (identity), `μ = Σqty` (quantity), `μ = total` (value). The duality
"partition by identity vs couple by value" is just "μ = count vs μ = total."

## 6. The `transfer` chokepoint — and why "by construction" was a cheat

You cannot manufacture a conservation law from an arbitrary function. `transfer`
preserving μ is, at bottom, a property of *its* implementation — convention
again. The honest framing is **trusted-computing-base size**:

| design                              | TCB (must be correct by convention)            | compiler forbids |
|-------------------------------------|------------------------------------------------|------------------|
| today: `split` + leaf discipline    | `split` **and every leaf**                     | nothing          |
| centralized `transfer` + `debug_assert` | the assert + the μ definition              | nothing (detects)|
| linear/move `transfer`              | the move kernel + unforgeable `Entry`          | dup, fabrication |

`split` and move-semantics each give **only half**:
- `split` (complement) → *no-loss* free; not no-dup/no-invent.
- move (un-`Clone` `Entry`) → *no-dup, no-invent* free; not no-loss (can `drop`).
Full conservation is the **join**, and still rests on a *total* kernel + an
*unforgeable* `Entry`.

**Irreducible floor:** μ is a semantic measure outside the program. The compiler
can machine-check mechanical conservation of opaque tokens (don't drop/dup/forge)
but can never check that a token *means* dollars. So:

```
conservation = (kernel correct) × (Entry unforgeable) × (μ faithful)
               └──── machine-checkable ────┘            └─ irreducible ─┘
```

## 7. How to force every domain through `transfer` (not roll its own S/O mutation)

Key observation: **conservation is a relation between two deltas — what left `S`
and what entered `O`. The only way to break it is for code to hold both handles
and correlate them wrongly.** So: make `transfer` the **only** code ever in scope
with both `&mut S` and `&mut O`. Deny the join, deny the violation.

### Mechanism A — capability denial via the trait signature (the real lever)
A leaf only *proposes*; it never returns `(O, S)`:

```rust
trait Leaf<E> {
    fn select(&self, view: &BagView<E>) -> Plan;   // Plan = Vec<Vec<Id>> (or Vec<(Id,Qty)>)
}

fn transfer<S: Drain, O: Place>(s: &mut S, o: &mut O, plan: Plan) {
    for cluster in plan {
        let mut unit = O::Unit::empty();
        for id in cluster {
            let item = s.remove(id).expect("plan named an absent id"); // no-invent
            unit.absorb(item);                                         // moved → no-dup
        }
        o.place(unit);                                                 // no drop path → no-loss
    }
}
```

No domain code is ever in scope with both handles; only `transfer` is. The leaf's
worst power is a *bad plan*, which `transfer` validates. Works under full
polymorphism because `Plan` is just ids — the universal currency of identity.

### Mechanism B — sealed types via module privacy (seal the bypass)
`Group`/`Resolution` get private fields and no public constructor; the sole
public factory is `transfer`. External crates physically cannot fabricate them.

### Residual gap (small)
Under polymorphism the domain *owns* `S`/`O`, so you can't seal their bytes;
`transfer` calls the domain's `Drain::remove` / `Place::absorb`. Residual trust =
those two one-line methods. But each runs **one-sided** (never sees the other
channel), so a bug can leak its own side's mass yet **cannot forge the
correlation** — the thing that *is* the law stays framework-only.

```
domain CANNOT correlate S and O       ← signature never grants both handles
domain CANNOT fabricate group/state   ← sealed types, private fields
domain CAN mis-implement remove/place ← residual TCB: 2 trivial one-sided methods
μ is the right measure                ← irreducible convention
```

### Next spike for this part
Draft `Leaf::select` + `Drain`/`Place` + generic `transfer`; re-express
`exact_1to1` (leaf) and `seq` (combinator) on top, and check the plan-returning
shape actually carries the existing matchers.
</content>
</invoke>

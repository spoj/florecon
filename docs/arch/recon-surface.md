# The `Recon` workspace surface — redesign

Status: proposal. Companion to `strategy-surface.md` (the algebra) and
`plugin-sdk.md` (the ABI). This rethinks the *stateful* facade in `src/recon.rs`
so the interface is **robust** (invariants by construction, atomic edits, no
silent frozen mutation), **clean** (one allocation type, no dead state, no magic
strings), **orthogonal** (every op does one thing; no two overlap), and
**expressive** (still covers the whole ABI `Cmd` set, with the bulk paths now
falling out as one parameterised primitive).

## 1. What the current surface gets wrong

`Recon<E>` today exposes ~13 mutators whose responsibilities overlap and whose
internal rules diverge (see the review in chat for line refs):

- **Conflated state.** A group's role is smeared across `status: {Live,Frozen}`
  *plus* the display string `origin`, where `origin == "unmatched"` secretly
  acts as control state in `cleanup_live_groups`.
- **Triplicated teardown.** The "drop emptied / re-mint the size-1 survivor"
  routine exists three times (`remove_many`, `cleanup_live_groups`,
  `pull_from_live`) with three different size-1 rules, so `ungroup` churns the
  ids of unrelated live singletons while `group_allocations` does not.
- **Dead state.** `StoredAlloc.original` is written in five places and never
  read for any decision or output.
- **Overlapping verbs.** `freeze` / `freeze_clean` / `freeze_singletons` are one
  concept; `group` / `group_allocations` are one concept with two *different*
  frozen policies; `breakup` / `remove_allocations` / `ungroup` are one concept.
- **Non-atomic mutation.** `group_allocations` validates availability up front,
  then mutates per-id in a second loop; a mid-loop failure leaves state edited
  and pulled mass dropped.
- **Silent frozen edits.** `remove` is the only mutator with no frozen guard;
  deleting a row silently rewrites (or un-freezes the survivor of) a signed-off
  group.

## 2. The model: two orthogonal axes

Every group is one allocation hyperedge over input ids. Its behaviour is fixed
by **two independent axes**, never by a string:

```rust
/// Does `solve` own this group, or does a human?
enum Lifecycle { Proposed, Pinned }

/// Provenance, for display only — never branched on.
enum Label {
    Strategy(String), // machine label from a leaf (e.g. "exact_1to1")
    Residual,         // a lone, unplaced lot
    Manual(String),   // a human decision's label
}
```

- `Lifecycle` is the **only** axis `solve` reads: it deletes every `Proposed`
  group and rebuilds them from the strategy, keeping `Pinned` groups verbatim.
- `Label` is pure provenance. A `Proposed` group of size 1 is a singleton
  whatever its label; when a teardown drops a proposed group to one member we
  set its label to `Residual` — an explicit marker, not the string `"unmatched"`.

The wire `Status {Live, Frozen}` is just `Lifecycle` projected (`Proposed→Live`,
`Pinned→Frozen`); `GroupOut.origin` is `Label` rendered. Internal control state
and external presentation are finally decoupled.

### One allocation type

`StoredAlloc { id, amount, original }` collapses to the existing
`strategy::Allocation { id, amount }` — the `original` field is deleted (dead),
and the same `Allocation` type now flows through strategy → recon → report. If
per-edge residual is ever a product need, it returns deliberately on the *wire*
(`AllocationOut`), not as silent internal ballast.

```rust
struct GroupRec {
    id: u64,
    lifecycle: Lifecycle,
    label: Label,
    reason: Option<String>,
    allocations: Vec<Allocation>, // signed mass; net is derived, not stored twice
}
```

## 3. The orthogonal operation set

Operations decompose onto four axes. Each method does exactly one thing.

### Axis 1 — the input ledger (mutates conserved truth)

```rust
fn upsert(&mut self, id: ExtId, item: E);
fn remove(&mut self, ids: &[ExtId]);
```

`remove` is the *one* operation allowed to touch a `Pinned` group, because
conservation forces it: a pinned group may not reference a deleted id. The rule
is explicit and total — **if `remove` would alter a pinned group, that whole
group is demoted to `Proposed` singletons** (its inputs changed, so the human
decision is void). No half-frozen survivors, no silent net rewrites.

### Axis 2 — the machine partition

```rust
fn solve(&mut self) -> Result<(), ApiError>;
```

Unchanged in spirit: dissolve all `Proposed`, run the strategy on
`items − pinned mass`, install fresh `Proposed` groups + `Residual` singletons,
then assert the conservation airlock. The airlock stays as the boundary guard.

### Axis 3 — lifecycle (promote/demote, never restructure)

```rust
fn pin(&mut self, group_id: u64) -> Result<(), ApiError>;      // targeted, typed error
fn pin_where(&mut self, pred: impl Fn(&GroupView<'_>) -> bool) -> usize; // bulk
fn unpin(&mut self, group_id: u64) -> Result<(), ApiError>;
```

`pin_where` is the single primitive that absorbs `freeze_clean` and
`freeze_singletons` — and anything else — because the predicate is a closure
(consistent with the strategy algebra's "closures over data"). The ABI's
concrete `Cmd` variants compile to predicates:

```rust
// FreezeClean { tol }
recon.pin_where(|g| g.is_match() && g.clean(tol));
// FreezeSingletons { ids }
recon.pin_where(|g| g.is_singleton() && g.contains_any(&ids));
```

### Axis 4 — the manual partition (restructure the proposed layer)

```rust
fn merge(&mut self, allocs: &[Allocation], label: &str, reason: Option<String>)
    -> Result<u64, ApiError>;          // one atomic constructor; result is Pinned
fn detach(&mut self, group_id: u64, ids: &[ExtId]) -> Result<(), ApiError>;
fn dissolve(&mut self, group_id: u64) -> Result<(), ApiError>;
```

- **`merge` is the single grouping constructor.** It always claims *exact* live
  mass and always yields a `Pinned` group (a manual group is a decision). It is
  **atomic**: validate every id's live availability, stage all pulls, commit
  once — a failure mutates nothing. There is **one** frozen policy: merge takes
  only live mass and never touches pinned groups; insufficient live mass (some
  is pinned elsewhere) is an honest `InsufficientLiveAmount`, never a blanket
  refusal and never a double-count.
- **`detach`** pulls ids out of a proposed group into singletons;
  **`dissolve`** breaks a whole proposed group. Both refuse `Pinned` groups
  uniformly (`unpin` first) — the asymmetry between the old `breakup`
  (allowed frozen) and `ungroup` (refused frozen) is gone.

The old row-id conveniences compose instead of multiplying methods:

```rust
fn live_mass(&self, ids: &[ExtId]) -> Vec<Allocation>; // all live mass of these ids

// Cmd::Group { ids }      → merge(&self.live_mass(&ids), label, reason)
// Cmd::Ungroup { ids }    → for each id's proposed group: detach(g, &[id])
```

### One private invariant restorer

All Axis-3/4 edits end by calling a single helper — replacing the three diverged
copies:

```rust
/// Restore the proposed-layer shape after an edit: drop emptied proposed
/// groups; relabel a proposed group that fell to one member as `Residual`.
/// Pinned groups and existing residual singletons keep their ids (stable
/// between solves — no churn).
fn normalize_proposed(&mut self);
```

Note it **no longer re-mints** size-1 survivors: a proposed singleton is a
proposed singleton regardless of how it got there, so its id stays stable until
the next `solve` re-mints the whole proposed layer anyway. The id-churn
inconsistency disappears at the root.

## 4. The shared read model

`GroupView<'a>` is the one inspection type — used by `pin_where` predicates,
external iteration, and as the basis for the report projection. Its metrics
reuse the strategy layer's vocabulary (`clean`, `size`) for consistency:

```rust
struct GroupView<'a> {
    id: u64,
    net: i64,
    lifecycle: Lifecycle,
    label: &'a Label,
    allocations: &'a [Allocation],
    reason: Option<&'a str>,
}
impl GroupView<'_> {
    fn size(&self) -> usize;
    fn is_singleton(&self) -> bool;     // size == 1
    fn is_match(&self) -> bool;         // size >= 2
    fn clean(&self, tol: Tol) -> bool;  // |net| within tol of the group scale
    fn contains(&self, id: ExtId) -> bool;
    fn contains_any(&self, ids: &[ExtId]) -> bool;
}

fn groups(&self) -> impl Iterator<Item = GroupView<'_>>; // public inspection
fn report(&self) -> Report;                              // sorted wire snapshot
```

## 5. Full method surface (before → after)

| Old (13) | New | Axis |
|---|---|---|
| `upsert` | `upsert` | ledger |
| `remove`, `remove_many` | `remove(&[ExtId])` | ledger |
| `solve` | `solve` | machine |
| `freeze` | `pin(id)` | lifecycle |
| `freeze_clean`, `freeze_singletons` | `pin_where(pred)` | lifecycle |
| `unfreeze` | `unpin(id)` | lifecycle |
| `group`, `group_allocations` | `merge(allocs, …)` (+ `live_mass`) | partition |
| `remove_allocations` | `detach(id, ids)` | partition |
| `breakup`, `ungroup` | `dissolve(id)` / `detach` | partition |
| `report`, `len`, `is_empty`, `replace_strategy` | unchanged | read/admin |

13 overlapping mutators → 9 orthogonal ones (`upsert`, `remove`, `solve`, `pin`,
`pin_where`, `unpin`, `merge`, `detach`, `dissolve`) plus read/admin. The ABI
`Cmd` enum is unchanged on the wire; only its `dispatch` arms change to call the
new primitives.

## 6. What each property buys

- **Robust**: `merge` atomic; pinned groups never silently edited (`remove`
  demotes wholesale, deliberately); conservation airlock retained; manual ops
  conserve by construction.
- **Clean**: one `Allocation` type end-to-end; `original` dead field deleted;
  `"unmatched"` magic string replaced by `Label::Residual`; one
  `normalize_proposed` instead of three diverged copies.
- **Orthogonal**: two independent axes (`Lifecycle` × `Label`); four operation
  families with no overlap; lifecycle never restructures and partition never
  re-pins.
- **Expressive**: `pin_where(pred)` generalises every bulk-freeze the FE needs
  and more; row-id conveniences compose from `live_mass` + `merge`/`detach`
  rather than spawning divergent methods.

## 7. Migration

1. Introduce `Lifecycle`, `Label`, `GroupView`; switch `GroupRec` to
   `Vec<Allocation>` (delete `StoredAlloc`).
2. Implement `normalize_proposed`; route `solve`/`detach`/`dissolve`/`merge`
   through it; delete `cleanup_live_groups` and `pull_from_live`.
3. Replace `freeze*`→`pin`/`pin_where`, `group*`→`merge`+`live_mass`,
   `breakup`/`remove_allocations`/`ungroup`→`dissolve`/`detach`.
4. Update `src/sdk/abi.rs` `dispatch` arms (wire `Cmd` unchanged); keep the
   `Status` projection (`Pinned→Frozen`, `Proposed→Live`).
5. `cargo test` (conformance kit unchanged — it only drives `upsert`/`solve`/
   `report`, all preserved).
```

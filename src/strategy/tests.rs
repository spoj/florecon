use super::*;

/// Rows are bare `i64`s: the value is the amount lane.
fn bag(vals: &[(Id, i64)]) -> Vec<Entry<i64>> {
    vals.iter().map(|&(id, v)| Entry::new(id, v)).collect()
}
fn amount(e: &Entry<i64>) -> i64 {
    e.data
}
fn ids(g: &Group) -> Vec<Id> {
    let mut v = g.members.clone();
    v.sort_unstable();
    v
}
/// Assert the resolution partitions the input ids (disjoint cover).
fn assert_conserved(r: &Resolution<i64>, input: &[Id]) {
    let mut seen: Vec<Id> = r
        .groups
        .iter()
        .flat_map(|g| g.members.iter().copied())
        .chain(r.residual.iter().map(|e| e.id))
        .collect();
    seen.sort_unstable();
    let mut want = input.to_vec();
    want.sort_unstable();
    assert_eq!(seen, want, "conservation: ids not a partition of input");
}

#[test]
fn exact_pairs_equal_and_opposite() {
    let b = bag(&[(1, 100), (2, -100), (3, 50)]);
    let r = exact_1to1(|_: &Entry<i64>| Some(0), amount).run(b);
    assert_conserved(&r, &[1, 2, 3]);
    assert_eq!(r.groups.len(), 1);
    assert_eq!(ids(&r.groups[0]), vec![1, 2]);
    assert_eq!(r.residual.len(), 1);
    assert_eq!(r.residual[0].id, 3);
}

#[test]
fn agg_net_keeps_balanced_bucket() {
    let b = bag(&[(1, 100), (2, -60), (3, -40), (4, 7)]);
    // one key for {1,2,3}, another for {4}
    let r = agg_net(
        |e: &Entry<i64>| if e.id == 4 { 1 } else { 0 },
        |g| g.net(amount) == 0,
    )
    .run(b);
    assert_conserved(&r, &[1, 2, 3, 4]);
    assert_eq!(r.groups.len(), 1);
    assert_eq!(ids(&r.groups[0]), vec![1, 2, 3]);
}

#[test]
fn agg_net_tolerance_is_an_inline_closure() {
    let b = bag(&[(1, 1000), (2, -997)]); // net 3
    let strict =
        agg_net(|_: &Entry<i64>| 0, |g| g.net(amount) == 0).run(bag(&[(1, 1000), (2, -997)]));
    assert_eq!(strict.groups.len(), 0);
    // "within 5 of zero" — author writes the inequality
    let loose = agg_net(|_: &Entry<i64>| 0, |g| g.net(amount).abs() <= 5).run(b);
    assert_eq!(loose.groups.len(), 1);
}

#[test]
fn signal_group_buckets_shared_tokens() {
    #[derive(Clone)]
    struct R {
        amt: i64,
        toks: Vec<u64>,
    }
    let b = vec![
        Entry::new(
            1,
            R {
                amt: 100,
                toks: vec![7],
            },
        ),
        Entry::new(
            2,
            R {
                amt: -100,
                toks: vec![7],
            },
        ),
        Entry::new(
            3,
            R {
                amt: 5,
                toks: vec![9],
            },
        ),
    ];
    let r = signal_group(|e: &Entry<R>| e.toks.clone(), |g| g.net(|e| e.amt) == 0, 16).run(b);
    assert_eq!(r.groups.len(), 1);
    assert_eq!(ids(&r.groups[0]), vec![1, 2]);
}

#[test]
fn cumulative_closes_clearing_segments() {
    // running balance clears after {1,2} and again after {3,4}
    let b = bag(&[(1, 50), (2, -50), (3, 30), (4, -30)]);
    let r = cumulative(|e: &Entry<i64>| e.id as i64, |g| g.net(amount) == 0).run(b);
    assert_conserved(&r, &[1, 2, 3, 4]);
    assert_eq!(r.groups.len(), 2);
}

#[test]
fn subset_sum_clears_many_to_one() {
    // anchor +100 clears against {-60, -40}
    let b = bag(&[(1, 100), (2, -60), (3, -40), (4, -7)]);
    let r = subset_sum(amount, 0, 4, 42).run(b);
    assert_conserved(&r, &[1, 2, 3, 4]);
    assert_eq!(r.groups.len(), 1);
    assert_eq!(ids(&r.groups[0]), vec![1, 2, 3]);
    assert_eq!(r.residual[0].id, 4);
}

#[test]
fn flow_emits_whole_row_clusters() {
    let spec = FlowSpec::new()
        .amount(|&v: &i64| v)
        .penalty(1000.0)
        .window(0)
        .block_key(|_| 0)
        .cost(|_a: &i64, _b: &i64| Some(1.0));
    // +100 must draw from -60 and -40: a partial-lot solver would split it,
    // but flow returns one whole-row cluster {1,2,3}.
    let b = bag(&[(1, 100), (2, -60), (3, -40)]);
    let r = flow(spec).run(b);
    assert_conserved(&r, &[1, 2, 3]);
    assert_eq!(r.groups.len(), 1);
    assert_eq!(ids(&r.groups[0]), vec![1, 2, 3]);
}

#[test]
fn seq_cascades_on_residual() {
    let b = bag(&[(1, 100), (2, -100), (3, 50), (4, -50)]);
    let s = seq(vec![
        exact_1to1(|_: &Entry<i64>| Some(0), amount),
        agg_net(|_: &Entry<i64>| 0, |g| g.net(amount) == 0),
    ]);
    let r = s.run(b);
    assert_conserved(&r, &[1, 2, 3, 4]);
    assert_eq!(r.groups.len(), 2); // exact takes one pair, agg nets the rest
}

#[test]
fn partition_by_shards_and_when_routes() {
    // shard by sign-of-id parity is silly; shard by a payload bucket instead.
    let b = bag(&[(1, 100), (2, -100), (3, 100), (4, -100)]);
    let s = partition_by(
        |e: &Entry<i64>| e.id % 2, // 1,3 vs 2,4 -> never nets within a shard
        |_| exact_1to1(|_: &Entry<i64>| Some(0), amount),
    );
    let r = s.run(b);
    assert_conserved(&r, &[1, 2, 3, 4]);
    assert_eq!(r.groups.len(), 0, "opposite signs land in different shards");

    let s2 = when(
        |e: &Entry<i64>| e.data.abs() == 100,
        exact_1to1(|_: &Entry<i64>| Some(0), amount),
    );
    let r2 = s2.run(bag(&[(1, 100), (2, -100), (3, 5)]));
    assert_eq!(r2.groups.len(), 1);
    assert!(r2.residual.iter().any(|e| e.id == 3));
}

#[test]
fn accept_if_dissolves_rejects_whole() {
    let b = bag(&[(1, 100), (2, -90)]); // net 10
    let r = accept_if(
        |g: &GroupView<i64>| g.net(amount).abs() <= 5,
        agg_net(|_: &Entry<i64>| 0, |_| true),
    )
    .run(b);
    assert_conserved(&r, &[1, 2]);
    assert_eq!(r.groups.len(), 0);
    assert_eq!(r.residual.len(), 2);
}

#[test]
fn soak_consumes_all_and_partition_makes_singletons() {
    let b = bag(&[(1, 5), (2, 7)]);
    let r = soak::<i64>("unmatched").run(b);
    assert_eq!(r.groups.len(), 1);
    assert_eq!(r.groups[0].size(), 2);

    let singles = partition_by(|e: &Entry<i64>| e.id, |_| soak("x")).run(bag(&[(1, 5), (2, 7)]));
    assert_eq!(singles.groups.len(), 2);
    assert!(singles.groups.iter().all(|g| g.size() == 1));
}

#[test]
fn labeled_stamps_reason() {
    let r = labeled("clean pair", exact_1to1(|_: &Entry<i64>| Some(0), amount))
        .run(bag(&[(1, 1), (2, -1)]));
    assert_eq!(r.groups[0].reason.as_deref(), Some("clean pair"));
}

#[test]
fn group_key_is_content_addressed_and_stable() {
    let g1 = Group::new(vec![3, 1, 2], "a");
    let g2 = Group::new(vec![1, 2, 3], "b"); // same members, different origin/order
    assert_eq!(g1.key(), g2.key(), "key depends only on the member set");
    let g3 = Group::new(vec![1, 2], "a");
    assert_ne!(g1.key(), g3.key());
}

#[test]
fn fixed_point_iterates_to_stability() {
    // A degenerate inner that never matches reaches a fixed point immediately.
    let r = fixed_point(identity(), 4).run(bag(&[(1, 1), (2, 2)]));
    assert_eq!(r.residual.len(), 2);
    assert_eq!(r.groups.len(), 0);
}

use super::*;

fn cell(m: &Measure, pairs: &[(Axis, Coord)]) -> Money {
    m.get(&Key::of(pairs))
}

#[test]
fn allocate_conserves_and_places() {
    let rent = Measure::build(&["geog", "time"], &[(&[("geog", 1), ("time", 1)], 1000)]);
    let rev = Measure::build(
        &["geog", "product", "time"],
        &[
            (&[("geog", 1), ("product", 10), ("time", 1)], 30),
            (&[("geog", 1), ("product", 11), ("time", 1)], 70),
        ],
    );
    let a = rent.allocate(&rev);
    assert_eq!(a.total(), 1000);
    assert!(a.pending().is_empty());
    assert_eq!(cell(&a, &[("geog", 1), ("product", 10), ("time", 1)]), 300);
    assert_eq!(cell(&a, &[("geog", 1), ("product", 11), ("time", 1)]), 700);
}

#[test]
fn undriven_mass_parks_on_any_in_cube() {
    let rent = Measure::build(
        &["geog", "time"],
        &[
            (&[("geog", 1), ("time", 1)], 1000),
            (&[("geog", 2), ("time", 1)], 500), // no driver
        ],
    );
    let rev = Measure::build(
        &["geog", "product", "time"],
        &[(&[("geog", 1), ("product", 10), ("time", 1)], 1)],
    );
    let a = rent.allocate(&rev);
    assert_eq!(a.total(), 1500);
    assert_eq!(a.pending().total(), 500);
    assert_eq!(cell(&a, &[("geog", 2), ("product", ANY), ("time", 1)]), 500);
}

#[test]
fn partial_overlap_driver() {
    let pool = Measure::build(
        &["geog", "time"],
        &[
            (&[("geog", 1), ("time", 1)], 100),
            (&[("geog", 1), ("time", 2)], 200),
        ],
    );
    let sqft = Measure::build(
        &["geog", "product"],
        &[
            (&[("geog", 1), ("product", 10)], 1),
            (&[("geog", 1), ("product", 11)], 3),
        ],
    );
    let a = pool.allocate(&sqft);
    assert_eq!(a.total(), 300);
    assert!(a.pending().is_empty());
    assert_eq!(cell(&a, &[("geog", 1), ("product", 10), ("time", 1)]), 25);
    assert_eq!(cell(&a, &[("geog", 1), ("product", 11), ("time", 1)]), 75);
    assert_eq!(cell(&a, &[("geog", 1), ("product", 11), ("time", 2)]), 150);
}

#[test]
fn marginalize_and_rekey() {
    let m = Measure::build(
        &["geog", "product"],
        &[
            (&[("geog", 1), ("product", 10)], 30),
            (&[("geog", 1), ("product", 11)], 70),
            (&[("geog", 2), ("product", 10)], 5),
        ],
    );
    let by_geog = m.marginalize(&["geog"]);
    assert_eq!(cell(&by_geog, &[("geog", 1)]), 100);
    assert_eq!(by_geog.total(), 105);
    let by_cat = m.rekey(|k| k.with("product", 1));
    assert_eq!(cell(&by_cat, &[("geog", 1), ("product", 1)]), 100);
    assert_eq!(by_cat.total(), 105);
}

#[test]
fn scenario_support_chain_to_cost_object() {
    // IT & finance support business team A; A sells only (customer 1, product X).
    let recipient_biza = 3u64;
    let it = Measure::build(&[], &[(&[], 1000)]); // scalar (empty-axis) measure
    let fin = Measure::build(&[], &[(&[], 500)]);
    let it_usage = Measure::build(&["recipient"], &[(&[("recipient", recipient_biza)], 1)]);
    let fin_usage = Measure::build(&["recipient"], &[(&[("recipient", recipient_biza)], 1)]);

    let support = it.allocate(&it_usage).combine(&fin.allocate(&fin_usage), |a, b| a + b);
    assert_eq!(support.get(&Key::of(&[("recipient", recipient_biza)])), 1500);

    let biza = support.marginalize(&[]); // collapse recipient -> scalar 1500
    let sales = Measure::build(&["customer", "product"], &[(&[("customer", 1), ("product", 10)], 1)]);
    let out = biza.allocate(&sales);
    assert_eq!(out.total(), 1500);
    assert_eq!(cell(&out, &[("customer", 1), ("product", 10)]), 1500);
}

#[test]
fn scenario_slice_warehouse_for_driver_and_pool() {
    // a full cube is heterogeneous facts; slice out the metric/year you need.
    // metric: rev=1, rent=2.
    let cube = Measure::build(
        &["metric", "year", "geog", "product"],
        &[
            (&[("metric", 1), ("year", 2024), ("geog", 1), ("product", 10)], 30),
            (&[("metric", 1), ("year", 2024), ("geog", 1), ("product", 11)], 70),
            (&[("metric", 1), ("year", 2023), ("geog", 1), ("product", 10)], 999), // other year
            (&[("metric", 2), ("year", 2024), ("geog", 1), ("product", ANY)], 1000), // rent
        ],
    );

    let rev = cube.slice(&[("metric", 1), ("year", 2024)]);
    assert_eq!(*rev.axes(), ["geog", "product"].into_iter().collect());
    assert_eq!(rev.total(), 100); // 2023 excluded

    let rent = cube.slice(&[("metric", 2), ("year", 2024)]).marginalize(&["geog"]);
    assert_eq!(rent.total(), 1000);

    let out = rent.allocate(&rev);
    assert_eq!(out.total(), 1000);
    assert_eq!(cell(&out, &[("geog", 1), ("product", 10)]), 300);
    assert_eq!(cell(&out, &[("geog", 1), ("product", 11)]), 700);
}

#[test]
fn scenario_reciprocal_services_then_to_cost_object() {
    // finance and IT support each other AND business team A; A sells (cust 1, prod X).
    let (fin0, it0) = (300i128, 300i128); // primary costs
    // recipients: finance=1, it=2, bizA=3
    let fin_usage = Measure::build(&["recipient"], &[(&[("recipient", 2)], 1), (&[("recipient", 3)], 1)]);
    let it_usage = Measure::build(&["recipient"], &[(&[("recipient", 1)], 1), (&[("recipient", 3)], 1)]);
    let send = |amt: Money, usage: &Measure| Measure::build(&[], &[(&[], amt)]).allocate(usage);

    let settle = |s: &(Money, Money)| {
        let (fin, it) = *s;
        let fin_recv = send(it, &it_usage).get(&Key::of(&[("recipient", 1)])); // IT -> finance
        let it_recv = send(fin, &fin_usage).get(&Key::of(&[("recipient", 2)])); // finance -> IT
        (fin0 + fin_recv, it0 + it_recv)
    };
    let (fin, it) = fixed_point((fin0, it0), settle, |a, b| a == b, 100);
    assert_eq!((fin, it), (600, 600));

    let biza_in = send(fin, &fin_usage).get(&Key::of(&[("recipient", 3)]))
        + send(it, &it_usage).get(&Key::of(&[("recipient", 3)]));
    assert_eq!(biza_in, fin0 + it0); // inter-service flows cancel -> 600

    let biza = Measure::build(&[], &[(&[], biza_in)]);
    let sales = Measure::build(&["customer", "product"], &[(&[("customer", 1), ("product", 10)], 1)]);
    let out = biza.allocate(&sales);
    assert_eq!(out.total(), 600);
    assert_eq!(cell(&out, &[("customer", 1), ("product", 10)]), 600);
}

#[test]
fn scenario_budget_completed_from_actual_shape() {
    let actual = Measure::build(
        &["customer", "product", "geog"],
        &[
            (&[("customer", 1), ("product", 10), ("geog", 1)], 60),
            (&[("customer", 1), ("product", 11), ("geog", 1)], 40),
            (&[("customer", 2), ("product", 10), ("geog", 2)], 100),
        ],
    );
    let budget = Measure::build(
        &["customer"],
        &[(&[("customer", 1)], 1000), (&[("customer", 2)], 500), (&[("customer", 3)], 300)],
    );

    let b = budget.allocate(&actual);
    assert_eq!(b.total(), 1800);
    assert_eq!(cell(&b, &[("customer", 1), ("product", 10), ("geog", 1)]), 600);
    assert_eq!(cell(&b, &[("customer", 1), ("product", 11), ("geog", 1)]), 400);
    assert_eq!(cell(&b, &[("customer", 2), ("product", 10), ("geog", 2)]), 500);
    assert_eq!(b.pending().total(), 300); // new customer parks on ANY/ANY
    assert_eq!(cell(&b, &[("customer", 3), ("product", ANY), ("geog", ANY)]), 300);

    let mix_p = actual.marginalize(&["product"]); // p10=160, p11=40
    let mix_g = actual.marginalize(&["geog"]); // g1=100, g2=100
    let done = b.rake("product", &mix_p).rake("geog", &mix_g);
    assert_eq!(done.total(), 1800);
    assert!(done.pending().is_empty());
    assert_eq!(cell(&done, &[("customer", 3), ("product", 10), ("geog", 1)]), 120);
    assert_eq!(cell(&done, &[("customer", 3), ("product", 11), ("geog", 2)]), 30);
}

#[test]
fn scenario_rent_completed_by_revenue() {
    let rent = Measure::build(
        &["geog", "time"],
        &[
            (&[("geog", 1), ("time", 1)], 1000),
            (&[("geog", 2), ("time", 1)], 500), // no revenue
        ],
    );
    let rev = Measure::build(
        &["geog", "product", "time"],
        &[
            (&[("geog", 1), ("product", 10), ("time", 1)], 30),
            (&[("geog", 1), ("product", 11), ("time", 1)], 70),
        ],
    );
    let a = rent.allocate(&rev);
    assert_eq!(a.total(), 1500);
    assert_eq!(cell(&a, &[("geog", 2), ("product", ANY), ("time", 1)]), 500);

    let mix_p = rev.marginalize(&["product"]); // p10=30, p11=70
    let done = a.rake("product", &mix_p);
    assert!(done.pending().is_empty());
    assert_eq!(cell(&done, &[("geog", 2), ("product", 10), ("time", 1)]), 150);
    assert_eq!(cell(&done, &[("geog", 2), ("product", 11), ("time", 1)]), 350);
}

#[test]
fn group_by_routes_per_company_and_recombines() {
    let cost = Measure::build(
        &["company", "time"],
        &[
            (&[("company", 1), ("time", 1)], 900),
            (&[("company", 2), ("time", 1)], 400),
        ],
    );
    let headcount = Measure::build(
        &["company", "dept"],
        &[(&[("company", 1), ("dept", 10)], 1), (&[("company", 1), ("dept", 11)], 2)],
    );
    let floor = Measure::build(
        &["company", "dept"],
        &[(&[("company", 2), ("dept", 10)], 3), (&[("company", 2), ("dept", 11)], 1)],
    );

    let out = cost.group_by("company", |co, sub| match co {
        1 => sub.allocate(&headcount),
        _ => sub.allocate(&floor),
    });

    assert_eq!(out.total(), 1300);
    assert_eq!(cell(&out, &[("company", 1), ("dept", 11), ("time", 1)]), 600);
    assert_eq!(cell(&out, &[("company", 2), ("dept", 10), ("time", 1)]), 300);
}

#[test]
fn allocate_rounds_exactly_to_the_penny() {
    let pool = Measure::build(&["geog"], &[(&[("geog", 1)], 10)]);
    let driver = Measure::build(
        &["geog", "product"],
        &[
            (&[("geog", 1), ("product", 10)], 1),
            (&[("geog", 1), ("product", 11)], 1),
            (&[("geog", 1), ("product", 12)], 1),
        ],
    );
    let a = pool.allocate(&driver);
    assert_eq!(a.total(), 10);
    assert!(a.pending().is_empty());
    let shares: Vec<Money> = [10u64, 11, 12]
        .iter()
        .map(|&p| cell(&a, &[("geog", 1), ("product", p)]))
        .collect();
    assert_eq!(shares, vec![4, 3, 3]);
}

#[test]
fn vacuum_coarsens_least_strict_dims_first() {
    let m = Measure::build(
        &["time", "product", "customer"],
        &[
            (&[("time", 1), ("product", 10), ("customer", 100)], 100),
            (&[("time", 1), ("product", 11), ("customer", 100)], 1),
            (&[("time", 1), ("product", 11), ("customer", 200)], 2),
        ],
    );
    let v = m.vacuum(10, &["customer", "product"]);
    assert_eq!(v.total(), 103);
    assert_eq!(cell(&v, &[("time", 1), ("product", 10), ("customer", 100)]), 100);
    assert_eq!(cell(&v, &[("time", 1), ("product", ANY), ("customer", ANY)]), 3);
    assert!(v.cells().all(|(k, _)| k.get("time") != Some(ANY))); // time protected
}

#[test]
fn vacuum_graduates_merged_cells_that_clear_eps() {
    let m = Measure::build(
        &["time", "customer"],
        &[
            (&[("time", 1), ("customer", 100)], 4),
            (&[("time", 1), ("customer", 200)], 5),
            (&[("time", 1), ("customer", 300)], 4),
        ],
    );
    let v = m.vacuum(10, &["customer"]);
    assert_eq!(v.total(), 13);
    assert_eq!(cell(&v, &[("time", 1), ("customer", ANY)]), 13);
}

#[test]
fn combine_select_partition() {
    let x = Measure::build(&["g"], &[(&[("g", 1)], 10), (&[("g", 2)], 20)]);
    let y = Measure::build(&["g"], &[(&[("g", 2)], 5), (&[("g", 3)], 7)]);
    let s = x.combine(&y, |a, b| a + b);
    assert_eq!(cell(&s, &[("g", 1)]), 10);
    assert_eq!(cell(&s, &[("g", 2)]), 25);
    assert_eq!(cell(&s, &[("g", 3)]), 7);

    let (lo, hi) = x.partition(|_, v| v < 15);
    assert_eq!(lo.total(), 10);
    assert_eq!(hi.total(), 20);
    let just1 = x.select(|k, _| k.get("g") == Some(1));
    assert_eq!(just1.total(), 10);
}

#[test]
fn largest_remainder_exact_and_negative() {
    let k = [Key::of(&[("a", 0)]), Key::of(&[("a", 1)]), Key::of(&[("a", 2)])];
    let keys: Vec<&Key> = k.iter().collect();
    assert_eq!(largest_remainder(100, &[1, 1, 1], 3, &keys), vec![34, 33, 33]);
    assert_eq!(largest_remainder(-100, &[1, 1, 1], 3, &keys).iter().sum::<Money>(), -100);
}

#[test]
fn ipf_fits_full_cube_to_two_marginal_budgets() {
    // last year actual seeds the association on (a,b,c); this year's budget is
    // known only on the marginals (a,b) and (b,c). IPF = alternately rescale the
    // cube to each target. `target.allocate(&current)` IS the fitting step: it
    // makes the cube's marginal equal `target` while keeping `current`'s shape.
    let seed = Measure::build(
        &["a", "b", "c"],
        &[
            (&[("a", 1), ("b", 1), ("c", 1)], 1),
            (&[("a", 1), ("b", 1), ("c", 2)], 1),
            (&[("a", 1), ("b", 2), ("c", 1)], 1),
            (&[("a", 1), ("b", 2), ("c", 2)], 1),
            (&[("a", 2), ("b", 1), ("c", 1)], 1),
            (&[("a", 2), ("b", 1), ("c", 2)], 1),
            (&[("a", 2), ("b", 2), ("c", 1)], 1),
            (&[("a", 2), ("b", 2), ("c", 2)], 1),
        ],
    );
    let target_ab = Measure::build(
        &["a", "b"],
        &[
            (&[("a", 1), ("b", 1)], 20),
            (&[("a", 1), ("b", 2)], 30),
            (&[("a", 2), ("b", 1)], 10),
            (&[("a", 2), ("b", 2)], 40),
        ],
    );
    let target_bc = Measure::build(
        &["b", "c"],
        &[
            (&[("b", 1), ("c", 1)], 12),
            (&[("b", 1), ("c", 2)], 18),
            (&[("b", 2), ("c", 1)], 28),
            (&[("b", 2), ("c", 2)], 42),
        ],
    );

    let step = |x: &Measure| target_bc.allocate(&target_ab.allocate(x));
    let same = |a: &Measure, b: &Measure| a.len() == b.len() && a.cells().all(|(k, v)| b.get(k) == v);
    let fitted = fixed_point(seed, step, same, 50);

    assert_eq!(fitted.total(), 100);
    // both marginal budgets are matched exactly — the IPF guarantee.
    assert!(same(&fitted.marginalize(&["a", "b"]), &target_ab));
    assert!(same(&fitted.marginalize(&["b", "c"]), &target_bc));
    // uniform seed -> the maximum-entropy (independence-given-b) fit.
    assert_eq!(cell(&fitted, &[("a", 1), ("b", 1), ("c", 1)]), 8);
    assert_eq!(cell(&fitted, &[("a", 2), ("b", 2), ("c", 2)]), 24);
}

#[test]
fn fixed_point_reciprocal_settles() {
    let step = |s: &(f64, f64)| (1000.0 + 0.10 * s.1, 500.0 + 0.20 * s.0);
    let (it, hr) = fixed_point(
        (0.0, 0.0),
        step,
        |a, b| (a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() < 1e-9,
        200,
    );
    assert!((it - 1050.0 / 0.98).abs() < 1e-3);
    assert!((hr - (500.0 + 0.20 * it)).abs() < 1e-6);
}

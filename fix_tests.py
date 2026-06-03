from pathlib import Path
t = Path('src/plan.rs').read_text()
import re
t = re.sub(r'#\[cfg\(test\)\]\nmod tests \{\}', '''#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn map() -> ColumnMap {
        let mut int_cols = HashMap::new();
        int_cols.insert("usd".into(), 0);
        int_cols.insert("day".into(), 1);
        int_cols.insert("class".into(), 2);
        int_cols.insert("objsub".into(), 3);
        int_cols.insert("native".into(), 4);
        let mut token_cols = HashMap::new();
        token_cols.insert("tokens".into(), 0);
        ColumnMap { int_cols, token_cols }
    }

    fn row(usd: i64, day: i64, objsub: i64, native: i64, tokens: &[u64]) -> PhysicalRow {
        PhysicalRow {
            ints: vec![usd, day, 0, objsub, native],
            tokens: vec![tokens.to_vec()],
        }
    }

    fn plan(root: PlanNode) -> Plan { Plan { primary: "usd".into(), root } }

    fn full_pipeline() -> Plan {
        plan(PlanNode::Seq { steps: vec![
            PlanNode::AggNet { key: "objsub".into(), tol: 0 },
            PlanNode::Exact {},
            PlanNode::Signal { signals: "tokens".into(), tol: 0, cap: 256 },
            PlanNode::Flow { day: "day".into(), tokens: "tokens".into(), penalty: 1000.0, window: 30, cost: CostSpec::default() },
        ]})
    }

    #[test]
    fn exact_pair_matches() {
        let mut s = Session::new(map());
        s.upsert(1, row(100, 1, 0, 999, &[])).unwrap();
        s.upsert(2, row(-100, 2, 0, -999, &[])).unwrap();
        let rep = s.solve(&full_pipeline()).unwrap();
        assert_eq!(rep.allocations.len(), 2);
    }
}''', t)
Path('src/plan.rs').write_text(t)

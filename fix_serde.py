from pathlib import Path
t = Path('src/plan.rs').read_text()
t = t.replace('pub map: ColumnMap,', '#[serde(default)]\n    pub map: ColumnMap,')
Path('src/plan.rs').write_text(t)

t2 = Path('src/wasm.rs').read_text()
t2 = t2.replace('map: ColumnMap,\n        plan: Plan,', '#[serde(default)]\n        map: ColumnMap,\n        plan: Plan,')
Path('src/wasm.rs').write_text(t2)

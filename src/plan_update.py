from pathlib import Path
import re

text = Path('src/plan.rs').read_text()

# Replace uses of LoweredRow/LoweredCell
text = re.sub(r'pub use crate::row::\{LoweredCell, LoweredRow\};\n', 'pub use crate::row::{PhysicalRow, ColumnMap};\n', text)
text = text.replace('LoweredRow', 'PhysicalRow')

# Replace ScalarRef/BoolRef with String
text = re.sub(r'pub use crate::expr::\{BoolExpr, BoolRef, ScalarExpr, ScalarRef\};\n', '', text)
text = text.replace('ScalarRef', 'String')
text = text.replace('BoolRef', 'String')
text = text.replace('use crate::lower::Row;\n', '')

# Replace Schema with ColumnMap in structures
text = text.replace('pub use crate::schema::{Column, Schema};\n', '')
text = text.replace('schema: Schema', 'map: ColumnMap')
text = text.replace('&self.schema', '&self.map')
text = text.replace('self.schema,', 'self.map,')

# Session modifications
text = re.sub(r'pub fn new\(schema: Schema\) -> Self \{\s*Session \{\s*schema,\s*rows: BTreeMap::new\(\),\s*\}\s*\}',
    'pub fn new(map: ColumnMap) -> Self {\n        Session {\n            map,\n            rows: BTreeMap::new(),\n        }\n    }', text)
text = re.sub(r'pub fn from_rows<I>\(schema: Schema, rows: I\) -> Result<Self, ApiError>\n\s*where\n\s*I: IntoIterator<Item = \(ExtId, Row\)>,',
    'pub fn from_rows<I>(map: ColumnMap, rows: I) -> Result<Self, ApiError>\n    where\n        I: IntoIterator<Item = (ExtId, PhysicalRow)>,', text)
text = re.sub(r'pub fn upsert\(&mut self, id: ExtId, row: Row\) -> Result<\(\), ApiError> \{\n\s*let lowered = row.lower\(&self\.schema\.kinds\(\), &self\.schema\.token_cfg\(\)\)\?;\n\s*self\.rows\.insert\(id, lowered\);\n\s*Ok\(\(\)\)\n\s*\}',
    'pub fn upsert(&mut self, id: ExtId, row: PhysicalRow) -> Result<(), ApiError> {\n        self.rows.insert(id, row);\n        Ok(())\n    }', text)

text = re.sub(r'pub struct SolveRequest \{\n\s*pub schema: Schema,\n\s*#\[serde\(default\)\]\n\s*pub rows: Option<Vec<\(ExtId, Row\)>>,\n\s*pub plan: Plan,\n\}',
    'pub struct SolveRequest {\n    pub map: ColumnMap,\n    pub plan: Plan,\n}', text)
text = re.sub(r'pub fn run\(self\) -> Result<Report, ApiError> \{\n\s*let session = Session::from_rows\(self\.schema, self\.rows\.unwrap_or_default\(\)\)\?;\n\s*session\.solve\(&self\.plan\)\n\s*\}',
    'pub fn run(self, rows: Vec<(ExtId, PhysicalRow)>) -> Result<Report, ApiError> {\n        let session = Session::from_rows(self.map, rows)?;\n        session.solve(&self.plan)\n    }', text)
text = re.sub(r'pub fn with_rows\(mut self, rows: Vec<\(ExtId, Row\)>\) -> Self \{\n\s*self.rows = Some\(rows\);\n\s*self\n\s*\}', '', text)

text = text.replace('pub fn new(schema: Schema, plan: Plan)', 'pub fn new(map: ColumnMap, plan: Plan)')
text = text.replace('let compiled = compile(&plan, &schema)?;', 'let compiled = compile(&plan, &map)?;')
text = text.replace('Workspace {\n            schema,\n', 'Workspace {\n            map,\n')
text = text.replace('pub fn schema(&self) -> &Schema {\n        &self.schema\n    }', 'pub fn map(&self) -> &ColumnMap {\n        &self.map\n    }')

# Fix upsert for workspace
text = re.sub(r'pub fn upsert\(&mut self, id: ExtId, row: Row\) -> Result<\(\), ApiError> \{\n\s*let lowered = row.lower\(&self\.schema\.kinds\(\), &self\.schema\.token_cfg\(\)\)\?;\n\s*self\.inner\.upsert\(id, lowered\);\n\s*Ok\(\(\)\)\n\s*\}',
    'pub fn upsert(&mut self, id: ExtId, row: PhysicalRow) -> Result<(), ApiError> {\n        self.inner.upsert(id, row);\n        Ok(())\n    }', text)

Path('src/plan.rs').write_text(text)

from pathlib import Path
import re

p = Path('src/plan.rs')
t = p.read_text()

t = re.sub(r'pub use crate::expr::\{String, String\};\n', '', t)
t = re.sub(r'pub fn from_rows<I>\(map: ColumnMap, rows: I\) -> Result<Self, ApiError>\n\s*where\n\s*I: IntoIterator<Item = \(ExtId, Row\)>,', 
           'pub fn from_rows<I>(map: ColumnMap, rows: I) -> Result<Self, ApiError>\n    where\n        I: IntoIterator<Item = (ExtId, PhysicalRow)>,', t)
t = re.sub(r'pub fn upsert\(&mut self, id: ExtId, row: Row\) -> Result<\(\), ApiError> \{\n\s*let lowered = row.lower\(&self\.map\.kinds\(\), &self\.map\.token_cfg\(\)\)\?;\n\s*self\.rows\.insert\(id, lowered\);\n\s*Ok\(\(\)\)\n\s*\}',
           'pub fn upsert(&mut self, id: ExtId, row: PhysicalRow) -> Result<(), ApiError> {\n        self.rows.insert(id, row);\n        Ok(())\n    }', t)

t = re.sub(r'pub fn upsert\(&mut self, id: ExtId, row: Row\) -> Result<\(\), ApiError> \{\n\s*let lowered = row.lower\(&self\.map\.kinds\(\), &self\.map\.token_cfg\(\)\)\?;\n\s*self\.inner\.upsert\(id, lowered\);\n\s*Ok\(\(\)\)\n\s*\}',
           'pub fn upsert(&mut self, id: ExtId, row: PhysicalRow) -> Result<(), ApiError> {\n        self.inner.upsert(id, row);\n        Ok(())\n    }', t)
           
t = t.replace('compiled.primary.eval(row)', 'row.int(compiled.primary)')
t = t.replace('pub fn schema(&self) -> &Schema {\n        &self.schema\n    }', 'pub fn map(&self) -> &ColumnMap {\n        &self.map\n    }')
t = t.replace('pub fn schema(&self) -> &Schema {\n        &self.map\n    }', 'pub fn map(&self) -> &ColumnMap {\n        &self.map\n    }')

# Fix schema inside session
t = re.sub(r'pub struct Session \{\n\s*schema: Schema,\n\s*rows: BTreeMap<ExtId, PhysicalRow>,\n\}',
           'pub struct Session {\n    map: ColumnMap,\n    rows: BTreeMap<ExtId, PhysicalRow>,\n}', t)

p.write_text(t)

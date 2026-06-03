from pathlib import Path
import re

t = Path('src/plan.rs').read_text()
t = re.sub(r'pub rows: Option<Vec<\(ExtId, Row\)>>,', '', t)
t = t.replace('#[serde(default)]\n    \n    pub plan: Plan,', 'pub plan: Plan,')

# Fix req.run
t = re.sub(r'pub fn run\(self\) -> Result<Report, ApiError> \{\n\s*let session = Session::from_rows\(self\.map, self\.rows\.unwrap_or_default\(\)\)\?;',
           'pub fn run(self, rows: Vec<(ExtId, PhysicalRow)>) -> Result<Report, ApiError> {\n        let session = Session::from_rows(self.map, rows)?;', t)

Path('src/plan.rs').write_text(t)

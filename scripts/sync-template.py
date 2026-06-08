#!/usr/bin/env python3
"""Sync the author seed into the host package as bundled template data.

`examples/starter-plugin/` is the single source of truth: CI builds and runs it
natively, so it cannot rot. The Python host ships a copy under
`florecon/_template/` (what `florecon new` scaffolds), with the in-repo `path`
dependency rewritten to a crates.io version dependency.

    python scripts/sync-template.py            # regenerate the bundled template
    python scripts/sync-template.py --check     # fail if it is out of sync (CI)
"""

from __future__ import annotations

import difflib
import filecmp
import shutil
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "examples/starter-plugin"
DST = ROOT / "hosts/python/src/florecon/_template"

# The crates.io dependency the scaffolded plugin uses (the repo seed uses a
# path dep, fenced by these sentinels in the seed's workspace Cargo.toml).
DEP_BEGIN = "# >>> florecon-dep"
DEP_END = "# <<< florecon-dep"
PUBLISHED_DEP = 'florecon = { version = "0.1", features = ["sdk"] }\n'

# Never ship build artifacts or a lockfile into the template.
EXCLUDE_DIRS = {"target", ".venv", "__pycache__"}
EXCLUDE_FILES = {"Cargo.lock"}


def _rewrite_dep(text: str) -> str:
    out, skipping = [], False
    for line in text.splitlines(keepends=True):
        stripped = line.strip()
        if stripped.startswith(DEP_BEGIN):
            out.append(PUBLISHED_DEP)
            skipping = True
            continue
        if stripped.startswith(DEP_END):
            skipping = False
            continue
        if not skipping:
            out.append(line)
    return "".join(out)


def _build(into: Path) -> None:
    if into.exists():
        shutil.rmtree(into)
    for src in sorted(SRC.rglob("*")):
        rel = src.relative_to(SRC)
        if any(part in EXCLUDE_DIRS for part in rel.parts):
            continue
        if src.is_dir():
            continue
        if src.name in EXCLUDE_FILES:
            continue
        dst = into / rel
        dst.parent.mkdir(parents=True, exist_ok=True)
        if src.name == "Cargo.toml" and DEP_BEGIN in src.read_text():
            dst.write_text(_rewrite_dep(src.read_text()))
        else:
            shutil.copy2(src, dst)


def _diff(a: Path, b: Path) -> list[str]:
    """Relative paths that differ between trees a and b (recursively)."""
    out: list[str] = []

    def walk(rel: str) -> None:
        cmp = filecmp.dircmp(a / rel if rel else a, b / rel if rel else b)
        for name in cmp.left_only:
            out.append(f"- {Path(rel) / name} (only in regenerated)")
        for name in cmp.right_only:
            out.append(f"+ {Path(rel) / name} (only in committed)")
        for name in cmp.diff_files:
            out.append(f"~ {Path(rel) / name}")
        for sub in cmp.common_dirs:
            walk(str(Path(rel) / sub))

    walk("")
    return out


def main() -> int:
    check = "--check" in sys.argv[1:]
    if check:
        with tempfile.TemporaryDirectory() as tmp:
            fresh = Path(tmp) / "_template"
            _build(fresh)
            diffs = _diff(fresh, DST)
            if diffs:
                print("bundled template is out of sync with examples/starter-plugin:")
                print("\n".join(diffs))
                # Show a unified diff for changed files to make the fix obvious.
                for d in diffs:
                    if d.startswith("~ "):
                        rel = d[2:]
                        a = (fresh / rel).read_text().splitlines(keepends=True)
                        b = (DST / rel).read_text().splitlines(keepends=True)
                        sys.stdout.writelines(
                            difflib.unified_diff(b, a, f"committed/{rel}", f"fresh/{rel}")
                        )
                print("\nrun: python scripts/sync-template.py")
                return 1
        print("bundled template is in sync")
        return 0
    _build(DST)
    print(f"synced {SRC.relative_to(ROOT)} -> {DST.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

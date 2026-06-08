"""The ``florecon`` command line: scaffold a plugin and drive the author loop.

This ships *with* the host (``uv add florecon-host`` / ``pip install
florecon-host``), so plugin authors get the dev experience without cloning the
florecon repo. ``new`` writes a plugin project; ``author`` / ``ship`` / ``check``
are thin, cross-platform wrappers over ``cargo`` (run from inside the project).

    florecon new my-recon
    cd my-recon
    florecon author          # fast native loop on data/sample.csv
    florecon ship            # build the production solver.wasm
"""

from __future__ import annotations

import argparse
import re
import shutil
import subprocess
import sys
from pathlib import Path

TEMPLATE = Path(__file__).resolve().parent / "_template"


def _slug(name: str) -> str:
    """A domain-safe identifier derived from the project name."""
    s = re.sub(r"[^a-z0-9]+", "-", name.lower()).strip("-")
    return s or "plugin"


def _cargo(args: list[str]) -> int:
    """Run cargo in the current directory, with a clear hint if it is missing."""
    if shutil.which("cargo") is None:
        print(
            "error: `cargo` not found. Install the Rust toolchain: https://rustup.rs",
            file=sys.stderr,
        )
        return 127
    try:
        return subprocess.call(["cargo", *args])
    except KeyboardInterrupt:
        return 130


def _uv(args: list[str]) -> int:
    """Run uv in the current directory, with a clear hint if it is missing."""
    if shutil.which("uv") is None:
        print(
            "error: `uv` not found. Install it: https://docs.astral.sh/uv/",
            file=sys.stderr,
        )
        return 127
    try:
        return subprocess.call(["uv", *args])
    except KeyboardInterrupt:
        return 130


def cmd_new(ns: argparse.Namespace) -> int:
    dest = Path(ns.name)
    if dest.exists():
        print(f"error: {dest} already exists", file=sys.stderr)
        return 1
    if not TEMPLATE.is_dir():
        print(
            "error: bundled template missing from this install of florecon-host",
            file=sys.stderr,
        )
        return 1

    shutil.copytree(TEMPLATE, dest)

    # Name the domain after the project so the wasm self-identifies.
    slug = _slug(dest.name)
    lib = dest / "solver/src/lib.rs"
    lib.write_text(lib.read_text().replace('"example.starter"', f'"{slug}"'))

    # Name the distributable wheel + its import package after the project, so the
    # data team writes `import my_recon` (not the seed's `import starter`).
    import_name = slug.replace("-", "_")
    pkg_src = dest / "package/src"
    seed_pkg = pkg_src / "starter"
    if seed_pkg.is_dir() and import_name != "starter":
        seed_pkg.rename(pkg_src / import_name)
        pyproj = dest / "package/pyproject.toml"
        pyproj.write_text(
            pyproj.read_text()
            .replace('name = "starter"', f'name = "{slug}"')
            .replace("src/starter", f"src/{import_name}")
        )

    print(f"created {dest}/  (domain: {slug})")
    print("next:")
    print(f"  cd {dest}")
    print("  florecon author        # edit solver/src/lib.rs, then re-run")
    print("  florecon package       # build the wasm + a distributable wheel")
    return 0


def cmd_author(ns: argparse.Namespace) -> int:
    return _cargo(["run", "--profile", "author", "-p", "harness", "--", ns.data])


def cmd_ship(_ns: argparse.Namespace) -> int:
    return _cargo(
        ["build", "-p", "solver", "--release", "--target", "wasm32-unknown-unknown"]
    )


WASM_REL = Path("target/wasm32-unknown-unknown/release/solver.wasm")


def cmd_package(_ns: argparse.Namespace) -> int:
    """Build the plugin wasm and package it as a universal wheel in dist/."""
    rc = cmd_ship(_ns)
    if rc:
        return rc
    if not WASM_REL.is_file():
        print(f"error: {WASM_REL} not found after build", file=sys.stderr)
        return 1
    # The package holds exactly one import dir under package/src/.
    src = Path("package/src")
    pkgs = sorted(p for p in src.glob("*") if p.is_dir()) if src.is_dir() else []
    if not pkgs:
        print("error: no package found under package/src/", file=sys.stderr)
        return 1
    shutil.copyfile(WASM_REL, pkgs[0] / "solver.wasm")
    return _uv(["build", "package", "--out-dir", "dist"])


def cmd_check(_ns: argparse.Namespace) -> int:
    return _cargo(["clippy", "-p", "solver"])


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="florecon",
        description="Scaffold and develop a florecon reconciliation plugin.",
    )
    sub = p.add_subparsers(dest="command", required=True)

    new = sub.add_parser("new", help="scaffold a new plugin project")
    new.add_argument("name", help="project directory to create")
    new.set_defaults(func=cmd_new)

    author = sub.add_parser(
        "author", help="fast native loop: build + run the strategy once on a sample"
    )
    author.add_argument(
        "data", nargs="?", default="data/sample.csv", help="CSV sample (default: %(default)s)"
    )
    author.set_defaults(func=cmd_author)

    ship = sub.add_parser("ship", help="build the production solver.wasm")
    ship.set_defaults(func=cmd_ship)

    package = sub.add_parser(
        "package", help="build the wasm and a distributable wheel (dist/*.whl)"
    )
    package.set_defaults(func=cmd_package)

    check = sub.add_parser("check", help="type-check the plugin (clippy)")
    check.set_defaults(func=cmd_check)

    return p


def main(argv: list[str] | None = None) -> int:
    ns = build_parser().parse_args(argv)
    return ns.func(ns)


if __name__ == "__main__":
    raise SystemExit(main())

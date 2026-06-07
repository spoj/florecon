#!/usr/bin/env bash
# Assert the three published packages share one version (unified versioning), and
# — if a release tag is given — that the tag matches it.
#
#   scripts/check-versions.sh           # consistency only (CI on push/PR)
#   scripts/check-versions.sh v0.2.0    # consistency + tag match (release guard)
set -euo pipefail

ver() { grep -m1 '^version' "$1" | sed -E 's/.*"([^"]+)".*/\1/'; }

fc=$(ver Cargo.toml)
de=$(ver florecon-derive/Cargo.toml)
py=$(ver hosts/python/pyproject.toml)
echo "florecon=$fc  florecon-derive=$de  florecon-host=$py"

if [[ "$fc" != "$de" || "$fc" != "$py" ]]; then
  echo "::error::version mismatch — all of florecon, florecon-derive, florecon-host must share one version"
  exit 1
fi

if [[ $# -ge 1 ]]; then
  tag="${1#v}"
  if [[ "$tag" != "$fc" ]]; then
    echo "::error::release tag '$1' != package version '$fc' — bump the three manifests to match the tag"
    exit 1
  fi
  echo "tag matches package version: $1 == $fc"
fi

echo "versions OK: $fc"

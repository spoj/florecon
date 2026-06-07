# Releasing

Publishing is automated by `.github/workflows/release.yml`: **publish a GitHub
Release and the three packages ship themselves** — `florecon` + `florecon-derive`
to crates.io, `florecon-host` to PyPI.

## Versioning model: unified

All three packages share **one** version. A release tag `vX.Y.Z` must equal the
`version` in:

- `Cargo.toml` (`florecon`)
- `florecon-derive/Cargo.toml`
- `hosts/python/pyproject.toml` (`florecon-host`)

CI enforces they agree (`versions` job); the release `guard` job enforces the tag
matches. `scripts/check-versions.sh` is the single source of that rule.

Because it is `0.x`, **a breaking change requires a minor bump** (0.1 → 0.2), and
`cargo-semver-checks` will fail CI if you break the API without bumping. When you
cross a minor (0.1 → 0.2), also bump the `florecon-derive` dependency requirement
in `Cargo.toml` (`florecon-derive = { version = "0.2", ... }`).

## One-time setup (do this once)

### 1. crates.io token → GitHub secret
- Create a token at <https://crates.io/settings/tokens> with scopes
  `publish-update` (and `publish-new` only if a crate name is new).
- Add it as a repo secret named **`CARGO_REGISTRY_TOKEN`**
  (Settings → Secrets and variables → Actions → New repository secret).

### 2. PyPI Trusted Publishing (no token needed)
On <https://pypi.org/manage/project/florecon-host/settings/publishing/> add a
GitHub publisher:
- Owner: `spoj` · Repository: `florecon`
- Workflow: `release.yml`
- Environment: `release`

### 3. GitHub environment `release`
Settings → Environments → **New environment** named `release`. Optional but
recommended: add yourself as a required reviewer so every publish needs a click.
Both publish jobs run in this environment.

## Cutting a release

```sh
# 1. bump the version in all three manifests (keep them identical)
v=0.1.1
sed -i -E "0,/^version = /s//version = \"$v\"  # ←/" Cargo.toml   # or edit by hand
#   edit: Cargo.toml, florecon-derive/Cargo.toml, hosts/python/pyproject.toml

# 2. sanity-check locally
bash scripts/check-versions.sh "v$v"
just lint && just test

# 3. commit + tag + push
git commit -am "release: v$v"
git tag "v$v" && git push origin master "v$v"

# 4. create the GitHub Release on that tag (UI or `gh`):
gh release create "v$v" --generate-notes
```

The release workflow then:
1. **guard** — tag must equal the manifests' version.
2. **crates** — publishes `florecon-derive` then `florecon`, skipping any version
   already on crates.io (idempotent re-runs).
3. **pypi** — builds the wheel/sdist and publishes via trusted publishing,
   `skip-existing: true`.

## What CI checks on every push / PR

- `rust` — `just lint` (clippy, warnings as errors) + `just test` (all features).
- `semver` — `cargo-semver-checks` vs the last crates.io release.
- `versions` — the three manifests agree.
- `hosts` — builds the interco wasm, then the Python golden replay + smoke and the
  JS host smoke (the wire contract holds end-to-end across both hosts).

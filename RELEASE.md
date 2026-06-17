# Release Operations

How crw-camofox ships, what to do when something breaks, and how to keep the pipeline honest.

This fork distributes **only** as a multi-arch Docker image on GHCR
(`ghcr.io/<owner>/<repo>`). Upstream's crates.io / npm / PyPI / APT / Homebrew
publishing is intentionally not part of this fork.

## Topology

`release-please` watches `main` for conventional commits and opens a Release PR. Merging it creates the `vX.Y.Z` tag, which fires `.github/workflows/release.yml`:

| Job               | Does                                                              |
| ----------------- | ---------------------------------------------------------------- |
| `release-context` | Derives + semver-validates the version from the tag.             |
| `check`           | `cargo fmt --check`, `clippy -D warnings`, build, `cargo test`.  |
| `publish-docker`  | Builds `linux/amd64,linux/arm64` and pushes to GHCR with `{{version}}`, `{{major}}.{{minor}}`, and `latest` tags. |

Two supporting Docker workflows:

- `docker-build.yml` — validates the multi-arch build (incl. the aarch64 cross-compile) on `release` events / `workflow_dispatch`; does not push.
- `docker-publish.yml` — manual `workflow_dispatch` to push an image to GHCR out-of-band, without cutting a release.

## Source of truth

These guards keep the workspace version-coherent and run on every PR via `preflight-publish.yml` (and partly `ci.yml`):

- **Tier order & publish flags:** `scripts/release/release_manifest.toml`, validated by `scripts/release/preflight.py`.
- **release-please extra-files validity + internal-pin completeness:** `scripts/release/audit_release_please_config.py`.
- **Internal crate-to-crate versions:** centralized in root `[workspace.dependencies]` (members inherit via `{ workspace = true }`); `scripts/check-internal-dep-versions.sh` enforces they equal the workspace version. Guarded against regression by `scripts/release/test_guards.py`.
- **Tag → version derivation:** `release-context` job in `release.yml` (semver-validated).

### Version cohesion — why the bump PR can't ship a stale surface

Every version surface (the workspace version and the 10 internal `[workspace.dependencies]` pins) must move in lockstep on a release. The historical break (v0.9.0, v0.12.0) was a stale surface slipping through because the **release-please bump PR is bot-authored and bot PRs cannot trigger workflows** — so the checks only ran post-tag.

Fixed by authoring the bump PR with an elevated token (`release-please.yml` → `with.token`), so it triggers `preflight-publish.yml` + `ci.yml` on its own head SHA before merge. **Operational requirement: `preflight-publish` must be a _Required_ status check in branch protection for the `main` / release-please branch** — that is what makes a red preflight *block the merge* (and therefore the tag) rather than just annotate it. Detection without the Required check is not prevention.

## Recovery runbooks

### The release failed mid-run

The pipeline is idempotent — re-run is safe. GHCR is content-addressed, so rebuild + push produces the same digest. Re-trigger a specific tag manually:

```bash
gh workflow run release.yml --ref main -f tag=vX.Y.Z
```

### The image shipped with a problem

GHCR tags are mutable by re-push, but treat published versions as immutable in practice: land the fix on `main`, let release-please open the `X.Y.Z+1` PR, merge it, and the next tag rebuilds + pushes. `latest` always tracks the newest tagged build.

## Secret rotation

| Secret            | Used by                                   | Rotation                                                                 |
| ----------------- | ----------------------------------------- | ------------------------------------------------------------------------ |
| `GH_DISPATCH_PAT` | `release-please.yml` (bump-PR author + tag creation) | GitHub fine-grained PAT with `contents:write` + `pull-requests:write` on this repo. Prefer migrating to a GitHub App installation token. |

`GITHUB_TOKEN` is auto-provisioned (used by `publish-docker` to push to GHCR) and does not need rotation.

## Adding a new crate

1. Add the crate to the workspace as usual.
2. Decide its tier in `scripts/release/release_manifest.toml` (published crates) or list it under `[unpublished]` with `[package] publish = false` in its Cargo.toml — preflight enforces the two agree.
3. If other crates depend on it, add **one** entry to the root `[workspace.dependencies]` table (`<crate> = { path = "crates/<crate>", version = "<workspace version>" }`) and have every consumer inherit it via `<crate> = { workspace = true }` (layer `features` / `optional` per-member; **never** inline `version =` in a member manifest). Add the matching `release-please-config.json` extra-file `$.workspace.dependencies.<crate>.version`. Run `bash scripts/check-internal-dep-versions.sh` and `python3 scripts/release/audit_release_please_config.py` — both fail red if the pin drifts or its config entry is missing.
4. Open a PR. `preflight-publish.yml` runs the full workspace check — green means the version surfaces are coherent.

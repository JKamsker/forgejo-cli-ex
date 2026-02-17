# Releasing / Publishing

## crates.io (automatic via CI)

This repo is configured to publish to crates.io on **push to `master`**, but only when:

- the version in `Cargo.toml` is **not** published yet, and
- the GitHub Actions secret `CARGO_REGISTRY_TOKEN` is set.

### 1) Create a crates.io token

On crates.io, create an API token with permission to publish crates.

### 2) Add the token to GitHub

Set the Actions secret:

```sh
gh secret set CARGO_REGISTRY_TOKEN --repo OWNER/forgejo-cli-ex
```

### 3) Release a new version

1. Bump the version in `Cargo.toml`.
2. Commit and push to `master`.
   - The `ci` workflow runs `cargo publish --locked` (skips if already published).
3. (Optional) Tag a GitHub release for binaries:
   - Create a tag `vX.Y.Z` and push it to trigger the `release` workflow.

## crates.io (manual)

```sh
cargo publish --dry-run --locked
cargo publish --locked
```

## Notes

- `cargo install forgejo-cli-ex` installs the `fj-ex` binary via `[[bin]] name = "fj-ex"` in `Cargo.toml`.

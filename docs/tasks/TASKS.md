# Tasks

- [x] Initialize repo scaffolding (git, .gitignore, docs/tasks)
- [x] Create Rust crate scaffold (`fj-ex`) + CLI skeleton
- [x] Implement target resolution (`--host/--repo/--remote`, git inference, `FJ_FALLBACK_HOST`) + unit tests
- [x] Implement shared UI creds/cookie store (`ui-creds.json`) + migration/repair + unit tests
- [x] Implement HTML helpers (csrf + data-* extraction + HTML decode) + unit tests
- [x] Implement UI session manager (reqwest + cookie jar, login, auto-relogin on `/user/login`)
- [x] Implement Actions UI endpoints (workflows, runs, run-view parsing, jobs, job meta)
- [x] Implement logs download (job/run) with stdout default + `--out-file/--out-dir`
- [x] Implement artifacts list/get
- [x] Implement cancel/rerun (no confirmation) + `--dry-run`
- [x] Implement `smoke-test` command (non-destructive)
- [x] Write `README.md` (security notes + examples)
- [x] Run `cargo fmt` + `cargo test` and fix issues
- [x] Implement login command (prompt/--password-stdin, store creds+cookies)

## Remote validation (local repo)

- [x] Build `fj-ex` and run `fj-ex smoke-test` inside a git repo with a Forgejo remote (host+repo inferred)
- [x] Run non-destructive `actions` commands against that Forgejo remote (workflows/runs/jobs/logs/artifacts)
- [x] Verify destructive endpoints are only hit with `--dry-run` (cancel/rerun)

## Auth + credential management

- [x] Add `auth` command group (`fj-ex auth login|status|...`) and keep `fj-ex login` as a legacy alias
- [x] Implement `fj-ex auth status` that checks against the host (session probe + optional relogin)
- [x] Implement credential CRUD commands (list/show/logout) for multiple hosts (from `ui-creds.json`)
- [x] Update error messages + README examples to reference `fj-ex auth login`
- [x] Run `cargo fmt` + `cargo test`

## GitHub + CI/CD

- [x] Create GitHub repo `forgejo-cli-ex` via `gh` and push current `master`
- [x] Add GitHub Actions CI workflow (fmt + test)
- [x] Add GitHub Actions release workflow (build + upload Windows/Linux/macOS artifacts on tag)
- [x] Push workflows to GitHub and verify they appear in `gh workflow list`
- [x] Verify CI is green
- [x] Create `v0.1.0` tag and verify release assets contain `fj-ex`/`fj-ex.exe`

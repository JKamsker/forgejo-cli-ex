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

## Remote validation (pandocs)

- [x] Build `fj-ex` and run `fj-ex smoke-test` against `C:\Users\Jonas\repos\work\dccx\src\pandocs`
- [x] Run non-destructive `actions` commands against pandocs Forgejo remote (workflows/runs/jobs/logs/artifacts)
- [x] Verify destructive endpoints are only hit with `--dry-run` (cancel/rerun)

## Auth + credential management

- [x] Add `auth` command group (`fj-ex auth login|status|...`) and keep `fj-ex login` as a legacy alias
- [x] Implement `fj-ex auth status` that checks against the host (session probe + optional relogin)
- [x] Implement credential CRUD commands (list/show/logout) for multiple hosts (from `ui-creds.json`)
- [ ] Update error messages + README examples to reference `fj-ex auth login`
- [ ] Run `cargo fmt` + `cargo test`

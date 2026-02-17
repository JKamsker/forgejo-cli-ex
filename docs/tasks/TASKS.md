# Tasks

- [x] Initialize repo scaffolding (git, .gitignore, docs/tasks)
- [x] Create Rust crate scaffold (`fj-ex`) + CLI skeleton
- [x] Implement target resolution (`--host/--repo/--remote`, git inference, `FJ_FALLBACK_HOST`) + unit tests
- [x] Implement shared UI creds/cookie store (`ui-creds.json`) + migration/repair + unit tests
- [x] Implement HTML helpers (csrf + data-* extraction + HTML decode) + unit tests
- [x] Implement UI session manager (reqwest + cookie jar, login, auto-relogin on `/user/login`)
- [x] Implement Actions UI endpoints (workflows, runs, run-view parsing, jobs, job meta)
- [ ] Implement logs download (job/run) with stdout default + `--out-file/--out-dir`
- [ ] Implement artifacts list/get
- [ ] Implement cancel/rerun (no confirmation) + `--dry-run`
- [ ] Implement `smoke-test` command (non-destructive)
- [ ] Write `README.md` (security notes + examples)
- [ ] Run `cargo fmt` + `cargo test` and fix issues

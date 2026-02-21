# DX/UX Improvement Tasks

Findings from a review of `fj-ex` CLI developer/user experience. Issues are grouped by theme with effort estimates.

---

## 1. Missing help text

Nearly all `actions` subcommands and their args lack doc comments, so `fj-ex actions --help` shows no descriptions.

- [ ] Add `/// ...` doc comment to `Actions(ActionsCommand)` in `Command` enum (`cli.rs`)
- [ ] Add `/// ...` doc comment to `SmokeTest(SmokeTestCommand)` in `Command` enum (`cli.rs`)
- [ ] Add `/// ...` doc comments to each variant of `ActionsSubcommand` (`Workflows`, `Runs`, `Jobs`, `Logs`, `Artifacts`, `Cancel`, `Rerun`) (`cli.rs`)
- [ ] Add `/// ...` `#[arg(help = "...")]` to each field inside `Cancel` (`run_index`, `dry_run`) (`cli.rs`)
- [ ] Add `/// ...` `#[arg(help = "...")]` to each field inside `Rerun` (`run_index`, `job_index`, `dry_run`) (`cli.rs`)
- [ ] Add `/// ...` `#[arg(help = "...")]` to fields in `ActionsLogsSubcommand::Run` (`run_index`, `latest`, `out_dir`, `max_jobs`) (`cli.rs`)
- [ ] Add `/// ...` `#[arg(help = "...")]` to fields in `ActionsLogsSubcommand::Job` (`run_index`, `job_index`, `attempt`, `out_file`) (`cli.rs`)
- [ ] Add `/// ...` `#[arg(help = "...")]` to fields in `ActionsArtifactsSubcommand::Get` (`run_index`, `artifact`, `out_file`) (`cli.rs`)
- [ ] Add `#[arg(long, default_value_t = 1_048_576, help = "...")]` to `log_download_max_bytes` in `SmokeTestCommand` (`cli.rs`)
- [ ] Verify `fj-ex --help`, `fj-ex actions --help`, and each subcommand `--help` show meaningful descriptions after changes

---

## 2. Bare invocation executes instead of printing help

`fj-ex actions jobs`, `fj-ex actions logs run`, and `fj-ex actions artifacts list` all execute immediately with no args (silently defaulting to the latest run). `logs run` is the most dangerous — it triggers multiple network requests and disk writes.

Root cause: `run_index: Option<i64>` + `latest: bool` are both optional, so clap sees a valid invocation and runs.

Fix: use a `clap::ArgGroup` with `required(true)` so that at least one of `--run-index` or `--latest` must be provided.

- [ ] Add `#[command(group(clap::ArgGroup::new("run_selector").required(true).args(["run_index", "latest"])))]` to `ActionsSubcommand::Jobs` (`cli.rs`)
- [ ] Add the same `ArgGroup` to `ActionsLogsSubcommand::Run` (`cli.rs`)
- [ ] Add the same `ArgGroup` to `ActionsArtifactsSubcommand::List` (`cli.rs`)
- [ ] Verify that bare `fj-ex actions jobs` now prints an error and help hint instead of executing
- [ ] Verify that `fj-ex actions jobs --latest` and `fj-ex actions jobs --run-index 50` still work

---

## 3. `cancel` and `rerun` missing `--latest`, `artifacts get` missing `--latest`

Every other run-scoped command supports `--latest`; `cancel`, `rerun`, and `artifacts get` require `--run-index` explicitly.

- [ ] Change `run_index: i64` to `run_index: Option<i64>` in `ActionsSubcommand::Cancel` (`cli.rs`)
- [ ] Add `latest: bool` field to `ActionsSubcommand::Cancel` (`cli.rs`)
- [ ] Add `ArgGroup` requiring `run_index | latest` to `Cancel` (`cli.rs`)
- [ ] Update `actions.rs` cancel handler to resolve `run_index` from latest when needed (same pattern as `jobs`) (`actions.rs`)
- [ ] Repeat the above four steps for `ActionsSubcommand::Rerun` (`cli.rs`, `actions.rs`)
- [ ] Change `run_index: i64` to `Option<i64>` in `ActionsArtifactsSubcommand::Get`, add `latest: bool`, add `ArgGroup` (`cli.rs`)
- [ ] Update `actions.rs` artifacts-get handler to resolve latest run index when needed (`actions.rs`)
- [ ] Verify `fj-ex actions cancel --latest --dry-run` works correctly

---

## 4. Target flags must precede the subcommand

`TargetArgs` is flattened onto `ActionsCommand`, not onto each subcommand. This means `--host`/`--repo`/`--remote` must come _before_ the subcommand name, which is the opposite of user expectation:

```
# Works:
fj-ex actions --host forge.example.com runs

# Fails with "unexpected argument":
fj-ex actions runs --host forge.example.com   ← what users type
```

Two options:

**Option A (preferred):** Flatten `TargetArgs` into each `ActionsSubcommand` variant individually. Verbose but standard clap behavior.
**Option B (lighter):** Keep the current structure but add a prominent note to `ActionsCommand`'s about text: `"Note: --host/--repo/--remote must appear before the subcommand name"`.

- [ ] Decide between Option A and Option B
- [ ] Implement chosen option
- [ ] Verify `fj-ex actions runs --host forge.example.com` either works (Option A) or prints a clear error with guidance (Option B)

---

## 5. `--max-jobs 0` means "unlimited" — counterintuitive

`cli.rs:191` — default is `0` and the check is `if max_jobs > 0`. Passing `--max-jobs 0` expecting to download nothing silently downloads everything.

- [ ] Add `#[arg(help = "Max jobs to download (0 = unlimited)")]` to `max_jobs` in `ActionsLogsSubcommand::Run` (`cli.rs`)
- [ ] Consider renaming to `--max-jobs-limit` or changing sentinel value — at minimum document the `0 = unlimited` convention clearly

---

## 6. Implicit `--run-index 0` / negative fallback to latest

`actions.rs:73-76` — the pattern `(Some(n), false) if n > 0 => n` silently treats `--run-index 0` and `--run-index -1` as "use latest" instead of erroring. This can mask scripting bugs where the index fails to parse.

- [ ] Add an explicit error branch for `run_index == Some(0)` or negative values: `return Err(eyre!("--run-index must be a positive integer"))` (`actions.rs`)
- [ ] Apply the same fix to `ActionsLogsSubcommand::Run` handler (`actions.rs`)
- [ ] Apply the same fix to `ActionsArtifactsSubcommand::List` handler (`actions.rs`)

---

## 7. `rerun` output is non-deterministic

`actions.rs:363-373` — success output is either a redirect URL (if Forgejo responds with `{"redirect": "..."}`) or the string `"Rerun requested."`, depending on server behavior. Scripts cannot reliably detect success by output.

- [ ] Always print a consistent success line, e.g. `"Rerun requested for run #{run_index}."`, and print the redirect URL on a second line only if present (or suppress it) (`actions.rs`)
- [ ] Consider adding `--json` output to `rerun` and `cancel` for scripting (`cli.rs`, `actions.rs`)

---

## 8. `runs` output lacks useful columns

`actions.rs:63-66` — text output is only `RunIndex` and `Url`. No status, branch, trigger event, or timestamp — information a developer needs to choose which run to investigate.

- [ ] Check whether `list_runs` in `ui_actions.rs` already returns status/branch/trigger data
- [ ] If not, extend the scraping/API call to retrieve those fields
- [ ] Add `Status`, `Branch`, and `CreatedAt` (or similar) columns to the text output of `fj-ex actions runs`
- [ ] Add the same fields to the `--json` output

---

## 9. `logs run` stdout separator destination undocumented

`actions.rs:223-226, 236` — `== job N (attempt M) :: name ==` separators go to stderr while log content goes to stdout. Correct for piping, but nowhere documented.

- [ ] Add a note to the `logs run` subcommand's help text: `"Job separators (== job N ==) are written to stderr; log content goes to stdout"` (`cli.rs`)

---

## Summary

| # | Issue | Effort |
|---|-------|--------|
| 1 | Missing help text across all actions subcommands | Low |
| 2 | Bare invocation executes instead of showing help | Low |
| 3 | `cancel`/`rerun`/`artifacts get` missing `--latest` | Low |
| 4 | Target flags must precede subcommand | Medium |
| 5 | `--max-jobs 0` = unlimited, undocumented | Low |
| 6 | `--run-index 0` silently uses latest | Low |
| 7 | `rerun` non-deterministic output | Low |
| 8 | `runs` output missing status/branch/timestamp | Medium |
| 9 | `logs run` separator destination undocumented | Low |

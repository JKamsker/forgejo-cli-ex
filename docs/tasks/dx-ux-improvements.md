# DX/UX Improvement Tasks

Findings from a review of `fj-ex` CLI developer/user experience. Issues are grouped by theme with effort estimates.

---

## 1. Missing help text

Nearly all `actions` subcommands and their args lack doc comments, so `fj-ex actions --help` shows no descriptions.

- [x] Add `/// ...` doc comment to `Actions(ActionsCommand)` in `Command` enum (`cli.rs`)
- [x] Add `/// ...` doc comment to `SmokeTest(SmokeTestCommand)` in `Command` enum (`cli.rs`)
- [x] Add `/// ...` doc comments to each variant of `ActionsSubcommand` (`Workflows`, `Runs`, `Jobs`, `Logs`, `Artifacts`, `Cancel`, `Rerun`) (`cli.rs`)
- [x] Add `/// ...` `#[arg(help = "...")]` to each field inside `Cancel` (`run_index`, `dry_run`) (`cli.rs`)
- [x] Add `/// ...` `#[arg(help = "...")]` to each field inside `Rerun` (`run_index`, `job_index`, `dry_run`) (`cli.rs`)
- [x] Add `/// ...` `#[arg(help = "...")]` to fields in `ActionsLogsSubcommand::Run` (`run_index`, `latest`, `out_dir`, `max_jobs`) (`cli.rs`)
- [x] Add `/// ...` `#[arg(help = "...")]` to fields in `ActionsLogsSubcommand::Job` (`run_index`, `job_index`, `attempt`, `out_file`) (`cli.rs`)
- [x] Add `/// ...` `#[arg(help = "...")]` to fields in `ActionsArtifactsSubcommand::Get` (`run_index`, `artifact`, `out_file`) (`cli.rs`)
- [x] Add `#[arg(long, default_value_t = 1_048_576, help = "...")]` to `log_download_max_bytes` in `SmokeTestCommand` (`cli.rs`)
- [x] Verify `fj-ex --help`, `fj-ex actions --help`, and each subcommand `--help` show meaningful descriptions after changes

---

## 2. Bare invocation executes instead of printing help

`fj-ex actions jobs`, `fj-ex actions logs run`, and `fj-ex actions artifacts list` all execute immediately with no args (silently defaulting to the latest run). `logs run` is the most dangerous — it triggers multiple network requests and disk writes.

Root cause: `run_index: Option<i64>` + `latest: bool` are both optional, so clap sees a valid invocation and runs.

Fix: use a `clap::ArgGroup` with `required(true)` so that at least one of `--run-index` or `--latest` must be provided.

- [x] Add `#[command(group(clap::ArgGroup::new("run_selector").required(true).args(["run_index", "latest"])))]` to `ActionsSubcommand::Jobs` (`cli.rs`)
- [x] Add the same `ArgGroup` to `ActionsLogsSubcommand::Run` (`cli.rs`)
- [x] Add the same `ArgGroup` to `ActionsArtifactsSubcommand::List` (`cli.rs`)
- [x] Verify that bare `fj-ex actions jobs` now prints an error and help hint instead of executing
- [x] Verify that `fj-ex actions jobs --latest` and `fj-ex actions jobs --run-index 50` still work

---

## 3. `cancel` and `rerun` missing `--latest`, `artifacts get` missing `--latest`

Every other run-scoped command supports `--latest`; `cancel`, `rerun`, and `artifacts get` require `--run-index` explicitly.

- [x] Change `run_index: i64` to `run_index: Option<i64>` in `ActionsSubcommand::Cancel` (`cli.rs`)
- [x] Add `latest: bool` field to `ActionsSubcommand::Cancel` (`cli.rs`)
- [x] Add `ArgGroup` requiring `run_index | latest` to `Cancel` (`cli.rs`)
- [x] Update `actions.rs` cancel handler to resolve `run_index` from latest when needed (same pattern as `jobs`) (`actions.rs`)
- [x] Repeat the above four steps for `ActionsSubcommand::Rerun` (`cli.rs`, `actions.rs`)
- [x] Change `run_index: i64` to `Option<i64>` in `ActionsArtifactsSubcommand::Get`, add `latest: bool`, add `ArgGroup` (`cli.rs`)
- [x] Update `actions.rs` artifacts-get handler to resolve latest run index when needed (`actions.rs`)
- [x] Verify `fj-ex actions cancel --latest --dry-run` works correctly

---

## 4. Target flags must precede the subcommand

`TargetArgs` is flattened onto `ActionsCommand`, not onto each subcommand. This means `--host`/`--repo`/`--remote` must come _before_ the subcommand name, which is the opposite of user expectation:

```text
# Works:
fj-ex actions --host forge.example.com runs

# Fails with "unexpected argument":
fj-ex actions runs --host forge.example.com   ← what users type
```

Two options:

**Option A (preferred):** Flatten `TargetArgs` into each `ActionsSubcommand` variant individually. Verbose but standard clap behavior.
**Option B (lighter):** Keep the current structure but add a prominent note to `ActionsCommand`'s about text: `"Note: --host/--repo/--remote must appear before the subcommand name"`.

- [x] Decide between Option A and Option B
- [x] Implement chosen option
- [x] Verify `fj-ex actions runs --host forge.example.com` either works (Option A) or prints a clear error with guidance (Option B)

---

## 5. `--max-jobs 0` means "unlimited" — counterintuitive

`ActionsLogsSubcommand::Run::max_jobs` — treating `--max-jobs 0` as "unlimited" is counterintuitive: users often expect `0` to mean "download nothing".

- [x] Add `#[arg(help = "Max jobs to download (0 = unlimited)")]` to `max_jobs` in `ActionsLogsSubcommand::Run` (`cli.rs`)
- [x] Consider renaming to `--max-jobs-limit` or changing sentinel value — at minimum document the `0 = unlimited` convention clearly

---

## 6. Implicit `--run-index 0` / negative fallback to latest

`resolve_run_index` — run index selection should reject `--run-index 0` / negative values rather than silently treating them as "use latest", to avoid masking scripting bugs.

- [x] Add an explicit error branch for `run_index == Some(0)` or negative values: `return Err(eyre!("--run-index must be a positive integer"))` (`actions.rs`)
- [x] Apply the same fix to `ActionsLogsSubcommand::Run` handler (`actions.rs`)
- [x] Apply the same fix to `ActionsArtifactsSubcommand::List` handler (`actions.rs`)

---

## 7. `rerun` output is non-deterministic

`actions.rs:363-373` — success output is either a redirect URL (if Forgejo responds with `{"redirect": "..."}`) or the string `"Rerun requested."`, depending on server behavior. Scripts cannot reliably detect success by output.

- [x] Always print a consistent success line, e.g. `"Rerun requested for run #{run_index}."`, and print the redirect URL on a second line only if present (or suppress it) (`actions.rs`)
- [x] Consider adding `--json` output to `rerun` and `cancel` for scripting (`cli.rs`, `actions.rs`)

---

## 8. `runs` output lacks useful columns

`actions.rs:63-66` — text output is only `RunIndex` and `Url`. No status, branch, trigger event, or timestamp — information a developer needs to choose which run to investigate.

- [x] Check whether `list_runs` in `ui_actions.rs` already returns status/branch/trigger data
- [x] If not, extend the scraping/API call to retrieve those fields
- [x] Add `Status`, `Branch`, and `CreatedAt` (or similar) columns to the text output of `fj-ex actions runs`
- [x] Add the same fields to the `--json` output

---

## 9. `logs run` stdout separator destination undocumented

`actions.rs:223-226, 236` — `== job N (attempt M) :: name ==` separators go to stderr while log content goes to stdout. Correct for piping, but nowhere documented.

- [x] Add a note to the `logs run` subcommand's help text: `"Job separators (== job N ==) are written to stderr; log content goes to stdout"` (`cli.rs`)

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

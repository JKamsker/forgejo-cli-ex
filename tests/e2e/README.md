# Docker E2E suite

End-to-end tests that run the real `fj-ex` binary against real servers: the
last three major versions of **Forgejo** and of **Gitea**, each in its own
Docker container with an Actions runner and a set of seeded demo
repositories.

## Targets

The single source of truth is the table in
[`harness/targets.rs`](harness/targets.rs):

| id           | server image                           | runner image                             | workflow dir |
|--------------|----------------------------------------|------------------------------------------|--------------|
| `forgejo-15` | `codeberg.org/forgejo/forgejo:15.0.11` | `data.forgejo.org/forgejo/runner:13.2.1` | `.forgejo`   |
| `forgejo-16` | `codeberg.org/forgejo/forgejo:16.0.7`  | `data.forgejo.org/forgejo/runner:13.2.1` | `.forgejo`   |
| `forgejo-17` | `codeberg.org/forgejo/forgejo:17.0.1`  | `data.forgejo.org/forgejo/runner:13.2.1` | `.forgejo`   |
| `gitea-1-25` | `docker.gitea.com/gitea:1.25.5`        | `gitea/runner:5.4.1`                     | `.gitea`     |
| `gitea-1-26` | `docker.gitea.com/gitea:1.26.4`        | `gitea/runner:5.4.1`                     | `.gitea`     |
| `gitea-1-27` | `docker.gitea.com/gitea:1.27.3`        | `gitea/runner:5.4.1`                     | `.gitea`     |

Each row also carries capability flags (`Caps`). A flag is off only when a
server genuinely lacks a feature; the affected check is then logged as
`SKIPPED ... capability ... is off` in the run summary. No target needs a
gate today.

Job containers use `buildpack-deps:bookworm-curl` (`runs-on: ubuntu-latest`).
The demo workflows use no remote actions (the artifact upload speaks the
artifact protocol with `curl`), so a run needs no access to github.com.

**Adding a version:** add a row to `TARGETS`, an `e2e_target!` line in
[`../e2e_docker.rs`](../e2e_docker.rs) and an entry in the `e2e` matrix of
`.github/workflows/ci.yml`. The non-ignored test `target_table_is_consistent`
fails if one of the three is missing.

## Demo repositories

Fixtures live in [`fixtures/repos/<name>/`](fixtures/repos): `repo.json`
(owner, description, extra branches), `main/` (files on `main`) and optional
`branches/<dir>/` overlays. A path segment `__WORKFLOW_DIR__` becomes
`.forgejo` or `.gitea`; `__FJEX_STACK__` in a workflow `name:` becomes a short
per-stack tag (runners name job containers after the task id and workflow
name, so the tag keeps parallel stacks on one Docker daemon apart).

| repository                 | purpose |
|----------------------------|---------|
| `fjex-demo/ci-artifacts`   | passing `ci.yml` on push, uploads `my-artifact` (artifacts, logs, smoke-test, remote inference) |
| `fjex-demo/ci-mixed`       | `mixed.yml`: a `pass` and a `fail` job; `fail` passes once the variable `FJEX_MIXED_SHOULD_PASS` is `true` (wait exit codes, `rerun --failed-only`) |
| `fjex-demo/dispatch-inputs`| `dispatch.yml` (`workflow_dispatch` inputs `greeting`/`outcome`, reads secret `FJEX_E2E_SECRET` and variable `FJEX_E2E_VARIABLE`) and `waiting.yml` (a job no runner can take) |
| `fjex-demo/slow-job`       | `slow.yml`: one tick per second (`logs job --follow`, `jobs --watch --json-lines`, cancel) |
| `e2e/pulls-demo`           | no workflows, branch `feature/greeting` (all `pulls` subcommands, empty runner queue) |

Seeding (org + repos through the REST API, contents pushed with the system
`git` over HTTP) is implemented once in [`harness/seed.rs`](harness/seed.rs).
Git runs with `GIT_CONFIG_GLOBAL=/dev/null`; credentials travel as an
`http.<url>.extraHeader` in `GIT_CONFIG_*` environment variables, never in
argv, remote URLs or `.git/config`.

Accounts: admin `e2e` / `e2e-password-1234`, reviewer `e2e-reviewer` /
`e2e-reviewer-password-1234` (both get a personal access token with scope
`all`; `harness::api::Api::create_token` mints scoped tokens for future
token-auth scenarios). The 2FA scenario creates `e2e-otp` and enrolls TOTP
through the web UI (`harness::ui::WebSession`).

## Running

Requirements: Linux (job containers use host networking), Docker, `git`.

```sh
# one target
FJ_EX_E2E=1 cargo test --locked --test e2e_docker e2e_forgejo_17_docker -- --ignored --nocapture --test-threads=1

# the same, selected by id (comma separated ids also work)
FJ_EX_E2E=1 FJ_EX_E2E_TARGET=gitea-1-27 cargo test --locked --test e2e_docker -- --ignored --nocapture

# all six targets, two at a time
FJ_EX_E2E=1 cargo test --locked --test e2e_docker -- --ignored --nocapture --test-threads=2
```

Test names: `e2e_forgejo_15_docker`, `e2e_forgejo_16_docker`,
`e2e_forgejo_17_docker`, `e2e_gitea_1_25_docker`, `e2e_gitea_1_26_docker`,
`e2e_gitea_1_27_docker`. A target run takes about one minute once the images
are pulled. Tests that `FJ_EX_E2E_TARGET` does not select print a skip message
and pass.

Environment variables:

| variable              | effect |
|-----------------------|--------|
| `FJ_EX_E2E=1`         | enable the suite (otherwise every test is a no-op) |
| `FJ_EX_E2E_TARGET`    | comma separated target ids to run (default: all) |
| `FJ_EX_E2E_KEEP=1`    | keep containers and the work dir after the run; remove later with `down.sh` |
| `FJ_EX_E2E_LOG_DIR`   | on failure, also write full server/runner/job container logs to `<dir>/<stack id>/` |

### Isolation and cleanup

Every run creates a stack id `fjex-e2e-<target>-<random>`; all its containers
(server, runner and, through the runner config, job containers) are named
with that prefix and labelled `fjex-e2e.stack=<id>`. The server listens on a
random free port bound to `127.0.0.1`. A drop guard removes the stack's
containers even when a scenario fails or panics; on failure it first prints
the last log lines of every container. Several targets can run in parallel on
one Docker daemon.

Scenarios return errors instead of panicking, so one failing scenario does not
hide the others; the end of each run prints a summary table with timings and
any notes or skipped checks. Every wait is a bounded poll.

## Manual stacks

Start one target with its demo repositories and use `fj-ex` against it by hand:

```sh
tests/e2e/up.sh --list          # targets
tests/e2e/up.sh forgejo-17      # boot + seed, prints URL, accounts and commands
tests/e2e/down.sh --list        # running stacks
tests/e2e/down.sh <stack-id>    # or: tests/e2e/down.sh --all
```

`up.sh` runs `cargo run --example e2e_stack`, which uses the same harness as
the tests. The stack keeps running until `down.sh`. Its work directory
(`target/fjex-e2e/<stack-id>/`) contains `stack.env` (URL, accounts, admin
token) and an isolated fj-ex credential store whose `keys.json` already holds
the admin API token:

```sh
export APPDATA=target/fjex-e2e/<stack-id>/appdata
printf '%s\n' 'e2e-password-1234' | fj-ex auth login --host http://localhost:<port> --username e2e --password-stdin
fj-ex actions --host http://localhost:<port> --repo fjex-demo/ci-artifacts runs
```

## Layout

```text
tests/e2e_docker.rs           one #[ignore] test per target + table consistency test
tests/e2e/mod.rs              run_target(): gating, stack boot, scenario run
tests/e2e/cli.rs              fj-ex driver (isolated APPDATA, timeouts, logging)
tests/e2e/harness/            shared with examples/e2e_stack.rs
  targets.rs                  the target table and capability flags
  stack.rs                    server + runner lifecycle, drop guard, log dumps
  seed.rs, fixtures.rs        demo repository seeding and templating
  api.rs, git.rs, ui.rs       REST API, system git, web UI (2FA enrollment)
  docker.rs, totp.rs, util.rs
tests/e2e/scenarios/          auth, tokens, runners, secrets/variables, runs/jobs/wait,
                              logs, artifacts, trigger, follow/watch, cancel/rerun,
                              pulls, smoke-test, remote inference
tests/e2e/fixtures/repos/     demo repositories
tests/e2e/up.sh, down.sh      manual stacks
```

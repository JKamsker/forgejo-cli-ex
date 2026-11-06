# Command Reference

All commands support `--host`/`-H`, `--repo`/`-r`, and `--remote`/`-R` for target resolution.
See the main [README](../README.md) for install and quickstart.

Tested against Forgejo 15–17 and Gitea 1.25–1.27 (see [tests/e2e](../tests/e2e/README.md)).
`--run-index` is the number in the run's web URL: the per-repository run index on Forgejo and
Gitea ≤ 1.25, the run id on Gitea ≥ 1.26 (which switched its Actions URLs to ids). `--job-index`
is always the 0-based position of the job in the run.

---

## auth

```sh
fj-ex auth login --host forge.example.com                      # interactive
fj-ex auth login --host forge.example.com --username my-user --password-stdin
fj-ex auth login --host forge.example.com --username my-user --password-stdin --otp-stdin
fj-ex auth login --host forge.example.com --token-stdin          # API token (automation, SSO)
fj-ex auth login --host forge.example.com --session-cookie-stdin # SSO browser session
fj-ex auth status --host forge.example.com
fj-ex auth list
fj-ex auth show   --host forge.example.com
fj-ex auth logout --host forge.example.com          # removes password, session and stored token
fj-ex auth clear-cookies --host forge.example.com   # keeps password/token; removes an imported session
```

Password via stdin (preferred over `--password`):

```sh
echo "my-password" | fj-ex auth login --host forge.example.com --username my-user --password-stdin
```

Two-factor passcodes are requested only when Forgejo redirects to the 2FA form. For scripts, use
`--otp-stdin`, `--otp`, or `FJ_OTP`; passcodes are not stored.

```sh
printf "my-password\n123456\n" | fj-ex auth login --host forge.example.com --username my-user --password-stdin --otp-stdin
```

Environment variable fallbacks:

- `FJ_USER`
- `FJ_PASS`
- `FJ_OTP`

Legacy alias: `fj-ex login --host forge.example.com`

### API token login

Store a Forgejo/Gitea personal access token instead of (or in addition to) a password:

```sh
printf '%s\n' "$TOKEN" | fj-ex auth login --host forge.example.com --token-stdin
fj-ex auth login --host forge.example.com --token-stdin          # prompts without echo on a terminal
fj-ex auth login --host forge.example.com --token "$TOKEN"       # works, but warns: visible in the process list
```

The token is validated with `GET /api/v1/user` before it is stored. A token without the
`read:user` scope is accepted with a warning (the username is then unknown). A stored password or
imported session for the same host is kept; the token is added next to it.

Instead of storing a token, commands also read one from the environment. Token precedence:

1. token stored by `fj-ex auth login --token-stdin` / `--token` for the base URL
2. `FJ_EX_TOKEN`, then `FORGEJO_TOKEN`, then `GITEA_TOKEN`
3. the official `fj` CLI's `keys.json` token for the host

Recommended scopes: `read:repository` (runs, jobs, logs, artifacts), plus `write:repository` for
cancel/rerun/dispatch/secrets/variables. `read:user` is optional (lets `auth status` show the user).

### SSO accounts: import a browser session

Accounts that sign in through OAuth2/OIDC/SAML have no password. Use a token (above), or import the
session cookie of a logged-in browser tab:

```sh
fj-ex auth login --host forge.example.com --session-cookie-stdin                 # paste when prompted (no echo)
printf '%s\n' "$COOKIE_VALUE" | fj-ex auth login --host forge.example.com --session-cookie-stdin
printf '%s\n' 'session=abc; persistent=def' | fj-ex auth login --host forge.example.com --session-cookie-stdin
printf '%s\n' "$COOKIE_VALUE" | fj-ex auth login --host forge.example.com --session-cookie-stdin --cookie-name my_cookie
```

Accepted input: a bare cookie value, `name=value`, or a full `Cookie:` header. For a bare value the
names `session` (current Forgejo), `i_like_gitea` (Gitea, older Forgejo) and `i_like_forgejo` are
tried; pass `--cookie-name` for a custom `[session] COOKIE_NAME`. The session is validated by
loading `/user/settings`; it is stored as a `session` login and used for UI commands. It cannot be
renewed automatically: once it expires, commands fail with "The imported browser session … has
expired" and suggest re-importing it or using a token. Including the remember-me cookie
(`persistent` on Forgejo, `gitea_incredible` on Gitea) lets the server re-create sessions until
that cookie expires. `auth clear-cookies` removes an imported session.

### Inspecting logins

```sh
fj-ex auth status --host forge.example.com --json   # credentialKind, sessionOk, tokenSource, tokenOk
fj-ex auth list                                    # e.g. alice@forge.example.com (password + token)
```

`auth status` checks the UI session (re-logging in with a stored password unless `--no-relogin`)
and validates any API token (stored, environment, or `fj` keys.json). It succeeds when at least one
credential works and warns about the others. Secrets are never printed.

---

## token mint nuget

Create a NuGet API key using the stored `fj-ex` username/password (Forgejo only creates access
tokens through the API with basic auth; SSO accounts create package tokens in the web UI):

```sh
fj-ex token mint nuget --host forge.example.com --owner my-org
fj-ex token mint nuget --host forge.example.com --owner my-team --json
```

Default behavior:

- Mints a new Forgejo personal access token with scope `write:package`
- Defaults `--owner` to the authenticated username
- Prints:
  - `FORGEJO_NUGET_USERNAME=...`
  - `FORGEJO_NUGET_SOURCE=https://.../api/packages/<owner>/nuget/index.json`
  - `FORGEJO_NUGET_API_KEY=...`

Read-only package key:

```sh
fj-ex token mint nuget --host forge.example.com --owner my-org --read-only
```

---

## token list

List personal access tokens for the authenticated user:

```sh
fj-ex token list --host forge.example.com
fj-ex token list --host forge.example.com --json
```

Default behavior:

- Resolves the authenticated user from the API token (stored by `fj-ex auth login --token-stdin`,
  `FJ_EX_TOKEN`/`FORGEJO_TOKEN`/`GITEA_TOKEN`, or `fj`'s keys.json)
- Lists token id, name, last eight characters, and scopes

---

## actions backends (`--backend`)

`fj-ex actions …` can use the web UI (cookie session) or the REST API (API token):

```sh
fj-ex actions --backend auto ...   # default
fj-ex actions --backend ui   ...   # always the web UI (password or imported session)
fj-ex actions --backend api  ...   # always the REST API (token, else basic auth with the stored password)
```

`auto` uses the UI when a password or session login is stored for the host, otherwise the API
token. When the UI session cannot be established (for example an expired imported session) and a
token is available, it falls back to the API and prints a warning.

REST API coverage verified against the servers' `/swagger.v1.json` and live instances:

| Operation (token only) | Forgejo 15 | Forgejo 16 | Forgejo 17 | Gitea 1.25 | Gitea 1.26 | Gitea 1.27 |
|---|---|---|---|---|---|---|
| `runs` (incl. `--workflow`, `--latest`) | yes | yes | yes | yes | yes | yes |
| `trigger` (dispatch; `--wait` needs `jobs`) | yes | yes | yes | yes | yes | yes |
| `jobs`, `jobs --watch`, `wait` | – | yes | yes | yes | yes | yes |
| `logs job` (incl. `--follow`), `logs run` | – | yes | yes | yes | yes | yes |
| `logs job --attempt <older attempt>` | – | yes | yes | – | – | yes |
| `artifacts list` / `artifacts get` | – | yes | yes | yes | yes | yes |
| `cancel` | – | yes | yes | – | – | – |
| `rerun`, `rerun --job-index` | – | – | yes | – | yes | yes |
| `rerun --failed-only` | – | – | yes (per job) | – | yes | yes |
| `workflows` | – | – | – | yes | yes | yes |
| `smoke-test` | UI only | UI only | UI only | UI only | UI only | UI only |

When an operation has no endpoint on the server, the command fails with an explanation and asks
for a UI login (`fj-ex auth login`, or `--session-cookie-stdin` for SSO) instead.

`--run-index` is always the number in the run's web URL. That is the per-repository run number on
Forgejo and Gitea <= 1.25, and the run id on Gitea >= 1.26; the API backend resolves it to the run
id automatically. `--job-index` is the 0-based position in the run's job list (web UI order).

---

## actions runs

```sh
fj-ex actions runs --repo owner/name --limit 20
fj-ex actions runs --repo owner/name --latest
fj-ex actions runs --repo owner/name --status failure
fj-ex actions runs --repo owner/name --workflow ci.yml
```

---

## pulls

All pull commands use an API token when available (see the token precedence
under `auth`) and otherwise fall back to the `fj-ex` password login's Basic
credentials. Error output never prints either credential. Human-readable output
replaces control characters in server-provided text (titles, refs, user names,
bodies); `--json` output is the unmodified API response.

| Command | Flags |
|---|---|
| `pulls list` | `--state open\|closed\|all` (default `open`), `--page N` (default 1), `--limit N` (default 20), `--json` |
| `pulls create` | `--head BRANCH`, `--base BRANCH`, `--title TEXT` (all required), `--body TEXT` |
| `pulls merge` | `--index N`, `--head-commit SHA`, `--title TEXT` (all required), `--force` |
| `pulls comment` | `--index N`, `--body TEXT` or `--body-file PATH` (`-` reads stdin), `--json` |
| `pulls comments` | `--index N`, `--page N` (default 1), `--limit N` (default 20), `--json` |
| `pulls review` | `--index N`, `--body TEXT` or `--body-file PATH` (`-` reads stdin), `--event comment\|approve\|request-changes` (default `comment`), `--commit SHA`, `--json` |
| `pulls reviews` | `--index N`, `--page N` (default 1), `--limit N` (default 20), `--json` |

List open pull requests:

```sh
fj-ex pulls list --repo owner/name
fj-ex pulls list --repo owner/name --state all --page 2 --limit 50 --json
```

Create a pull request (prints `Created pull request #<n> <url>`):

```sh
fj-ex pulls create --repo owner/name --head release-fix --base main --title "fix: release control"
fj-ex pulls create --repo owner/name --head release-fix --base main --title "fix: release control" --body "Details"
```

Merge a pull request. `--head-commit` must be the PR's current source commit, so
a head that changed after review is not merged. `--force` bypasses required
checks and approvals; use it only with explicit operator authorization:

```sh
fj-ex pulls merge --repo owner/name --index 42 --head-commit <sha> --title "fix: release control"
fj-ex pulls merge --repo owner/name --index 42 --head-commit <sha> --title "fix: release control" --force
```

A merge rejected with HTTP 405 usually means the caller lacks merge permission
or branch protection (approvals/checks) is not satisfied; the error names the
credential source that was used.

Post and verify a normal PR comment:

```sh
fj-ex pulls comment --repo owner/name --index 42 --body "Review complete"
fj-ex pulls comment --repo owner/name --index 42 --body-file review.md --json
fj-ex pulls comments --repo owner/name --index 42 --json
fj-ex pulls comments --repo owner/name --index 42 --page 2 --limit 50
```

Submit and verify a review object. `COMMENT` is the default and does not
approve or request changes:

```sh
fj-ex pulls review --repo owner/name --index 42 --body-file review.md
fj-ex pulls review --repo owner/name --index 42 --body-file review.md --commit <sha>
fj-ex pulls reviews --repo owner/name --index 42 --json
```

Use `--body-file -` to read generated content from standard input. Approval and
change-request events are explicit:

```sh
fj-ex pulls review --repo owner/name --index 42 --body "Approved" --event approve
fj-ex pulls review --repo owner/name --index 42 --body "Please fix" --event request-changes
```

This Forgejo deployment exposes normal PR comments through
`/issues/{index}/comments`; review objects use `/pulls/{index}/reviews`.

---

## actions jobs

```sh
fj-ex actions jobs --repo owner/name --latest
fj-ex actions jobs --repo owner/name --latest --watch
fj-ex actions jobs --repo owner/name --run-index 50 --watch --json
fj-ex actions jobs --repo owner/name --run-index 50 --watch --json-lines
```

Flags: `--run-index N` or `--latest` (with optional `--workflow FILE`), `--watch`,
`--watch-interval SECONDS` (default 2), `--header`/`--no-header`, `--json`, `--json-lines`.

`--watch` is a display mode for the jobs listing: it refreshes the job table until the run is
finished. For automation, prefer `actions wait`.

With `--watch --json`, a single pretty-printed JSON snapshot is printed once the run is finished.
`--json-lines` (requires `--watch`, conflicts with `--json`) streams one compact JSON object per
line: the initial snapshot, every snapshot whose job states changed, and the final one
(`"complete": true`). Each snapshot carries `runStatus` (from the Actions run list, which is
authoritative when the run page's job data lags), `terminalObservations`, and `observedAtUnixMs`.
A terminal `runStatus` must be observed twice before the watch stops. If the run cannot be found in
the run list, a warning is printed to stderr once and the job statuses decide instead.

---

## actions wait

Wait for a workflow run to reach a terminal state:

```sh
fj-ex actions wait --repo owner/name --run-index 50
fj-ex actions wait --repo owner/name --latest --workflow ci.yml --timeout 30m
fj-ex actions wait --repo owner/name --latest --workflow ci.yml --interval 5 --json
```

Wait for one job within a run:

```sh
fj-ex actions wait --repo owner/name --run-index 50 --job-index 0
```

Flags: `--run-index N` or `--latest` (with optional `--workflow FILE`), `--job-index N` (0-based),
`--interval SECONDS` (default 2), `--timeout DURATION`, `--json`.

Like `actions jobs --watch`, `actions wait` treats a terminal status from the Actions run list
(observed twice) as authoritative over a lagging job snapshot; if the run cannot be found in the
run list, it warns once on stderr and falls back to the job statuses. With `--job-index`, the wait
also ends as soon as that job finishes while the rest of the run continues; `cancelling` jobs and
jobs still `blocked` on their `needs` keep the wait going.

Exit behavior:

- Exits `0` when the selected run/job reaches a non-failing terminal state.
- Exits nonzero when the selected run/job finishes as `failure`, `canceled`/`cancelled`, or `blocked`
  (for example a run that stays blocked waiting for approval).
- Exits nonzero when `--timeout` is reached.

Timeouts accept plain seconds or a unit suffix: `30s`, `10m`, `1h`, `1d`.

`actions wait` operates at run/job level. Forgejo step state is not currently exposed as a stable
CLI target by `fj-ex`, so there is no step-level wait flag.

---

## actions logs

Download a single job's log to stdout:

```sh
fj-ex actions logs job --repo owner/name --latest --job-index 0
fj-ex actions logs job --repo owner/name --run-index 50 --job-index 0
fj-ex actions logs job --repo owner/name --run-index 50 --job-index 0 --follow --follow-interval 5
```

`--follow` polls the job log every `--follow-interval` seconds (default 5) and prints only the
newly available log bytes on each poll until the run reaches a terminal state; it is meant for
watching progress, not for capturing the complete existing log. If the log is replaced (for example
by a rerun), a notice is written to stderr and the current log is replayed. The command exits
nonzero when the run finishes as `failure`, `canceled`, or `blocked`. Without `--follow`, the
complete log is downloaded once (to stdout or `--out-file`). Other flags: `--attempt N` (defaults
to the latest attempt).

Download all logs for a run to files:

```sh
fj-ex actions logs run --repo owner/name --run-index 50 --out-dir .tmp/forgejo-logs/run-50
fj-ex actions logs run --repo owner/name --latest --workflow ci.yml --out-dir .tmp/forgejo-logs/latest-ci
```

---

## actions artifacts

```sh
fj-ex actions artifacts list --repo owner/name --latest
fj-ex actions artifacts get  --repo owner/name --run-index 50 --artifact my-artifact --out-file .tmp/artifact.zip
```

---

## actions cancel / rerun

Both execute immediately. Use `--dry-run` to preview.

```sh
fj-ex actions cancel --repo owner/name --run-index 50 --dry-run
fj-ex actions cancel --repo owner/name --run-index 50

fj-ex actions rerun  --repo owner/name --run-index 50 --dry-run
fj-ex actions rerun  --repo owner/name --latest --failed-only
```

---

## actions trigger

Dispatch a `workflow_dispatch` event:

```sh
fj-ex actions trigger --repo owner/name --workflow ci.yml --ref main
fj-ex actions trigger --repo owner/name --workflow ci.yml --ref main --wait --timeout 30m
```

Flags: `--workflow FILE` (required, e.g. `ci.yml`), `--ref REF` (default `main`; a branch name or a
full `refs/...` ref), `--input KEY=VALUE` (repeatable), `--dry-run`, `--wait`, `--interval SECONDS`
(with `--wait`; default 2), `--timeout DURATION` (with `--wait`), `--json`.

```sh
fj-ex actions trigger --repo owner/name --workflow deploy.yml --ref refs/tags/v1.2.0 --input env=staging --dry-run
```

With `--wait`, `fj-ex` records the latest matching run before dispatch, polls for a newer run for
the selected workflow on the dispatched ref, then waits for that run to finish exactly like
`actions wait`. If several new runs match, the oldest is chosen, because the dispatched run is
created before the dispatch request returns. The run list can only be filtered by workflow file
name, so for an all-digit `--workflow` id the wait considers new runs of any workflow on the
dispatched ref. `--interval` controls polling frequency and `--timeout` covers the whole wait. The
same failure and timeout exit behavior as `actions wait` applies; with `--json`, a single JSON
document with the final result (including a `trigger` object) is printed.

---

## actions secrets set / actions variables set

Create or replace a repository Actions secret or variable. The value is read from standard input
only (`--value-stdin` is required); literal values on the command line are intentionally
unsupported, and the value is never echoed. One trailing newline (`\n` or `\r\n`) is stripped, and
an empty value is rejected.

```sh
printf '%s' "$HARBOR_PASSWORD" | fj-ex actions secrets set --repo owner/name --name HARBOR_PASSWORD --value-stdin
printf '%s' "$HARBOR_USERNAME" | fj-ex actions variables set --repo owner/name --name HARBOR_USERNAME --value-stdin
```

Flags: `--name NAME` (ASCII letters, digits, and underscores, not starting with a digit),
`--value-stdin`. Both use the REST API with the stored `fj-ex` UI credentials (`fj-ex auth login`).
On failure only the HTTP status is reported, never the server's response body.

---

## actions runners

These commands use the Forgejo REST API and require an API token: one stored with
`fj-ex auth login --token-stdin`, `FJ_EX_TOKEN` / `FORGEJO_TOKEN` / `GITEA_TOKEN`, or the token
stored by the official `fj` CLI.

Runner registration tokens:

```sh
# Repo scope (default if --repo is set / inferred)
fj-ex actions runners token --repo owner/name

# Global scope (admin endpoints)
fj-ex actions runners token --scope global

# Org scope
fj-ex actions runners token --scope org --org my-org

# User scope
fj-ex actions runners token --scope user
```

Runner jobs:

```sh
# Show waiting jobs for the repo
fj-ex actions runners jobs --repo owner/name --waiting

# Filter by runner labels (repeatable; sent as labels=a,b)
fj-ex actions runners jobs --scope global --label self-hosted --label linux
```

Gitea has no runner job queue endpoint; there `runners jobs` reads Gitea's `/actions/jobs`
listing instead, reports queued jobs as `waiting` and applies `--label` client-side.

If you get a "No Forgejo API token found" error, provide a token:

```sh
printf '%s\n' "$TOKEN" | fj-ex auth login --host forge.example.com --token-stdin
# or authenticate via `fj`
fj --host forge.example.com auth login
fj --host forge.example.com auth add-key <USER>
```

Token store location (read by `fj-ex`):

```text
%APPDATA%/Cyborus/forgejo-cli/data/keys.json   # Windows
~/.local/share/Cyborus/forgejo-cli/data/keys.json  # Linux
```

---

## actions workflows

```sh
fj-ex actions workflows --repo owner/name
```

---

## smoke-test

Non-destructive end-to-end validation (downloads logs, checks artifacts, etc.):

```sh
fj-ex smoke-test --repo owner/name
fj-ex smoke-test --repo owner/name --out-dir /tmp/fj-ex-smoke

# Also available under actions:
fj-ex actions smoke-test --repo owner/name
```

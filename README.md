# forgejo-cli-ex

[![crates.io](https://img.shields.io/crates/v/forgejo-cli-ex)](https://crates.io/crates/forgejo-cli-ex)
[![license](https://img.shields.io/crates/l/forgejo-cli-ex)](LICENSE)
[![GitHub](https://img.shields.io/badge/GitHub-JKamsker%2Fforgejo--cli--ex-181717?logo=github)](https://github.com/JKamsker/forgejo-cli-ex)
[![Codeberg](https://img.shields.io/badge/Codeberg-JKamsker%2Fforgejo--cli--ex-2185D0?logo=codeberg)](https://codeberg.org/JKamsker/forgejo-cli-ex)
[![Blog](https://img.shields.io/badge/blog-how%20fj--ex%20was%20built-orange)](https://blog.kamsker.at/blog/how-fj-ex-was-built)

`fj-ex` extends the official Forgejo CLI (`fj`) with functionality that requires hitting **web UI endpoints** — full action logs, artifacts, cancel/rerun, workflow dispatch, and more.
With an API token it can also drive Forgejo/Gitea Actions through the **REST API**, so automation does not need a password.

## Install

```sh
cargo install forgejo-cli-ex
fj-ex --help
```

## Quickstart

```sh
# Login (interactive)
fj-ex auth login --host forge.example.com

# Login with 2FA in scripts (stdin lines are password, then passcode)
printf "my-password\n123456\n" | fj-ex auth login --host forge.example.com --username my-user --password-stdin --otp-stdin

# Token-only automation (no password): store a scoped API token ...
printf '%s\n' "$FORGEJO_TOKEN" | fj-ex auth login --host forge.example.com --token-stdin
# ... or just export it; every command picks it up
export FORGEJO_TOKEN=...   # also FJ_EX_TOKEN or GITEA_TOKEN
fj-ex actions logs job --repo owner/name --run-index 50 --job-index 0
fj-ex actions rerun    --repo owner/name --run-index 50 --failed-only

# SSO accounts: use a token (above), or import the browser session cookie
fj-ex auth login --host forge.example.com --session-cookie-stdin   # paste the cookie value when prompted

# Mint a NuGet API key (requires `fj auth login` + `fj-ex auth login`)
fj-ex token mint nuget --host forge.example.com --owner my-org
fj-ex token list --host forge.example.com

# List recent runs
fj-ex actions runs --repo owner/name --latest

# Wait for CI to finish, failing the command if the run fails
fj-ex actions wait --repo owner/name --latest --workflow ci.yml --timeout 30m

# Stream job logs to stdout
fj-ex actions logs job --repo owner/name --latest --job-index 0

# Follow a running job without reprinting its prior log bytes
fj-ex actions logs job --repo owner/name --latest --job-index 0 --follow

# Cancel / rerun (preview first with --dry-run)
fj-ex actions cancel --repo owner/name --run-index 50 --dry-run
fj-ex actions rerun  --repo owner/name --latest --failed-only

# Create or replace a repository Actions secret without exposing it in argv/logs
printf '%s' "$HARBOR_PASSWORD" | \
  fj-ex actions secrets set --repo owner/name --name HARBOR_PASSWORD --value-stdin
printf '%s' "$HARBOR_USERNAME" | \
  fj-ex actions variables set --repo owner/name --name HARBOR_USERNAME --value-stdin

# Runner registration token + queued jobs (requires `fj auth login`)
fj-ex actions runners token --repo owner/name
fj-ex actions runners jobs  --repo owner/name --waiting

# Dispatch a workflow and wait for the created run
fj-ex actions trigger --repo owner/name --workflow ci.yml --ref main --wait --timeout 30m

# Create a release-control PR (merge requires the expected source commit)
fj-ex pulls create --repo owner/name --head release-fix --base release/canary --title "fix: release control"
fj-ex pulls merge --repo owner/name --index 42 --head-commit <sha> --title "fix: release control" --force

# Review a PR without approving it (the default review event is COMMENT)
fj-ex pulls list --repo owner/name --state open --json
fj-ex pulls comment --repo owner/name --index 42 --body-file review.md
fj-ex pulls review --repo owner/name --index 42 --body-file review.md
```

> `--host` can be omitted — `fj-ex` infers it from the current repo's git remotes, or falls back to `$FJ_FALLBACK_HOST`.

## Commands

| Group | What it does |
|---|---|
| `auth` | Login (password, API token, or imported SSO browser session), logout, status, list saved logins |
| `token` | Mint and list personal access tokens |
| `actions workflows` | List workflows of a repository |
| `actions runs` | List workflow runs (filter by status, workflow, latest) |
| `actions jobs` | List jobs for a run, optionally `--watch`; `--watch --json-lines` streams one compact JSON snapshot per line |
| `actions wait` | Wait for a run or job to complete, with script-friendly exit codes |
| `actions logs` | Download complete job/run logs; `logs job --follow` polls and prints only newly available bytes until the run finishes |
| `actions artifacts` | List / download artifacts |
| `actions cancel` | Cancel a running workflow |
| `actions rerun` | Rerun a workflow (optionally `--failed-only`) |
| `actions trigger` | Dispatch a `workflow_dispatch` event; `--wait` waits for the dispatched run like `actions wait` |
| `actions secrets set` | Create or replace a repository Actions secret from stdin; never accepts or echoes a literal value |
| `actions variables set` | Create or replace a repository Actions variable from stdin; never accepts or echoes a literal value |
| `pulls list` | List pull requests with state/page/limit filters |
| `pulls comment` | Post a normal PR comment through the issue-comments API |
| `pulls comments` | Read normal PR comments for verification |
| `pulls review` | Submit a review; defaults to `COMMENT`, with explicit approval events only |
| `pulls reviews` | Read review objects for verification |
| `pulls create` | Create a pull request through the REST API |
| `pulls merge` | Merge a pull request only at an expected source commit (`--head-commit`) |
| `actions runners` | Runner tokens + queued jobs (REST API; needs an API token) |
| `smoke-test` | Non-destructive end-to-end validation |

Full command reference with all flags: [docs/commands.md](docs/commands.md)

## CI observation contract

Use `actions wait`, `actions runs`, or `actions jobs --watch --json` to decide
whether a run has finished (`actions wait` and `trigger --wait` apply the same
rules below). The JSON job snapshot includes `runStatus` and `observedAtUnixMs`;
`runStatus` from the Actions list is authoritative when the run page still has
stale job data. A terminal status must be observed twice because Forgejo can
briefly report a previous terminal state while scheduling a rerun attempt. Use `actions logs job --follow` only for progress, and download
the complete job or run log after a terminal failure to diagnose it. Logs never
decide the terminal state.

## Pull request review contract

`pulls comment` uses Forgejo's issue-comment endpoint because this instance
exposes normal pull request comments at `/issues/{index}/comments`, not
`/pulls/{index}/comments`. `pulls review` uses `/pulls/{index}/reviews` and
defaults to the non-decision `COMMENT` event. Use `--event approve` or
`--event request-changes` only when that decision is explicitly authorized.
For generated or multi-line text, prefer `--body-file`; `--body-file -` reads
from stdin and keeps the body out of the process list.

## Target resolution

Most commands accept `--host`/`-H`, `--repo`/`-r`, or `--remote`/`-R`. If omitted, `fj-ex` infers host and repo from the current directory's git remotes.

## Credentials

`fj-ex auth login` stores one login per instance (keyed by the normalized base URL, including a
sub-path such as `https://host/gitea`). A login is one of:

| Kind | How | UI commands | REST API commands | Renewal |
|---|---|---|---|---|
| `password` | `fj-ex auth login` (prompt, `--password-stdin`, `--otp-stdin`) | yes | basic auth | automatic re-login |
| `token` | `fj-ex auth login --token-stdin` | no | yes | none needed |
| `session` | `fj-ex auth login --session-cookie-stdin` (SSO) | yes | no | re-import when it expires |

A stored token is kept next to a password or session login, so one host can have both
(`auth list` shows e.g. `password + token`). `auth status`, `auth list` and `auth show` report the
credential kind and token source but never print token or cookie values.

### API token sources

REST API calls use the first token found, in this order:

1. a token stored with `fj-ex auth login --token-stdin` (or `--token`) for that base URL,
2. the environment variables `FJ_EX_TOKEN`, then `FORGEJO_TOKEN`, then `GITEA_TOKEN`,
3. the token stored by the official `fj` CLI (`keys.json`, written by `fj auth login`).

Commands that historically used the `fj-ex` password for REST calls (`actions secrets/variables set`,
`actions trigger`, `pulls`) fall back to basic auth with the stored password when no token is found.
Environment tokens are sent to whichever host a command targets, so only export them in shells
that talk to that instance. Prefer `--token-stdin` over `--token` (which is visible in the
process list and prints a warning).

Token scopes: `read:repository` is enough to list runs/jobs and download logs/artifacts;
`write:repository` is needed for cancel, rerun, dispatch and secrets/variables; `read:user` lets
`auth login`/`auth status` show the token's user (optional).

### Actions backends

`fj-ex actions …` talks to the web UI with a cookie session, or to the REST API with a token
(`--backend auto|ui|api`, default `auto`). `auto` uses the UI when a password or session login is
stored — exactly as before — and otherwise the API token. If the UI session is unusable (e.g. an
imported session expired) and a token is available, `auto` falls back to the API with a warning.

Forgejo/Gitea web routes do not accept API tokens, and REST coverage of Actions differs by server:

| Operation (token only) | Forgejo 15 | Forgejo 16 | Forgejo 17 | Gitea 1.25 | Gitea 1.26 | Gitea 1.27 |
|---|---|---|---|---|---|---|
| `runs`, `trigger` | yes | yes | yes | yes | yes | yes |
| `jobs`, `wait`, `logs job/run` | – | yes | yes | yes | yes | yes |
| `logs job --attempt <older>` | – | yes | yes | – | – | yes |
| `artifacts list/get` | – | yes | yes | yes | yes | yes |
| `cancel` | – | yes | yes | – | – | – |
| `rerun` (run, `--job-index`, `--failed-only`) | – | – | yes | – | yes | yes |
| `workflows` | – | – | – | yes | yes | yes |

Unsupported combinations fail with a message naming the missing endpoint and pointing to a UI
login. `smoke-test` always uses the UI. `--run-index` is always the number shown in the run's web
URL; `fj-ex` maps it to the API run id (on Gitea >= 1.26 the web URL already shows the id).

### SSO (OAuth2/OIDC/SAML) accounts

SSO users have no local password, so the password form login cannot work. Either:

- **recommended:** create a personal access token in the web UI (Settings > Applications) and use
  `fj-ex auth login --token-stdin` or `FORGEJO_TOKEN`; or
- import a logged-in browser session: copy the session cookie from the browser's developer tools
  (`session` on current Forgejo, `i_like_gitea` on Gitea and older Forgejo) and run
  `fj-ex auth login --host … --session-cookie-stdin`. Bare values, `name=value` and a full
  `Cookie:` header are accepted; `--cookie-name` handles a custom `[session] COOKIE_NAME`. The
  session is validated against `/user/settings` before it is stored. It cannot be renewed: when it
  expires, commands say so — re-import it or switch to a token. If you logged in with
  "remember me", also import that cookie (`persistent` on Forgejo, `gitea_incredible` on Gitea),
  e.g. by pasting the full Cookie header: the server then re-creates expired sessions by itself
  until the remember-me cookie expires.

### Files

Credentials and cookies are stored in plaintext at:

```text
%APPDATA%\Cyborus\forgejo-cli\data\ui-creds.json   # Windows
~/.local/share/Cyborus/forgejo-cli/data/ui-creds.json  # Linux
```

The official `fj` CLI's token store (read as the last token source) is:

```text
%APPDATA%\Cyborus\forgejo-cli\data\keys.json   # Windows
~/.local/share/Cyborus/forgejo-cli/data/keys.json  # Linux
```

`fj-ex token mint nuget` needs a password login (the API only creates tokens with basic auth).
Downloaded logs and artifacts may contain secrets — handle accordingly.

If your account uses two-factor authentication, `fj-ex auth login` prompts for the current passcode after the password step. For noninteractive use, pass `--otp`, `--otp-stdin`, or set `FJ_OTP`. The passcode is not stored; only the resulting UI cookies are stored so later commands can reuse the session until Forgejo expires it.

## License

LGPL-3.0-or-later

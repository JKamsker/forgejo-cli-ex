# forgejo-cli-ex

[![crates.io](https://img.shields.io/crates/v/forgejo-cli-ex)](https://crates.io/crates/forgejo-cli-ex)
[![license](https://img.shields.io/crates/l/forgejo-cli-ex)](LICENSE)
[![GitHub](https://img.shields.io/badge/GitHub-JKamsker%2Fforgejo--cli--ex-181717?logo=github)](https://github.com/JKamsker/forgejo-cli-ex)
[![Codeberg](https://img.shields.io/badge/Codeberg-JKamsker%2Fforgejo--cli--ex-2185D0?logo=codeberg)](https://codeberg.org/JKamsker/forgejo-cli-ex)
[![Blog](https://img.shields.io/badge/blog-how%20fj--ex%20was%20built-orange)](https://blog.kamsker.at/blog/how-fj-ex-was-built)

`fj-ex` extends the official Forgejo CLI (`fj`) with functionality that requires hitting **web UI endpoints** — full action logs, artifacts, cancel/rerun, workflow dispatch, and more.

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

# Mint a NuGet API key (requires `fj auth login` + `fj-ex auth login`)
fj-ex token mint nuget --host forge.example.com --owner my-org
fj-ex token list --host forge.example.com

# List recent runs
fj-ex actions runs --repo owner/name --latest

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
| `auth` | Login, logout, status, list saved sessions |
| `token` | Mint and list personal access tokens |
| `actions runs` | List workflow runs (filter by status, workflow, latest) |
| `actions jobs` | List jobs for a run, optionally `--watch` |
| `actions logs` | Download complete job/run logs; `logs job --follow` prints only newly appended bytes |
| `actions artifacts` | List / download artifacts |
| `actions cancel` | Cancel a running workflow |
| `actions rerun` | Rerun a workflow (optionally `--failed-only`) |
| `actions trigger` | Dispatch a `workflow_dispatch` event |
| `actions secrets set` | Create or replace a repository Actions secret from stdin; never accepts or echoes a literal value |
| `actions variables set` | Create or replace a repository Actions variable from stdin; never accepts or echoes a literal value |
| `pulls list` | List pull requests with state/page/limit filters |
| `pulls comment` | Post a normal PR comment through the issue-comments API |
| `pulls comments` | Read normal PR comments for verification |
| `pulls review` | Submit a review; defaults to `COMMENT`, with explicit approval events only |
| `pulls reviews` | Read review objects for verification |
| `pulls create/merge` | Create pull requests and merge an expected source commit through the REST API |
| `actions runners` | Runner tokens + queued jobs (REST API; uses `fj` token store) |
| `smoke-test` | Non-destructive end-to-end validation |

Full command reference with all flags: [docs/commands.md](docs/commands.md)

## CI observation contract

Use `actions runs` or `actions jobs --watch --json` to decide whether a run has
finished. The JSON job snapshot includes `runStatus` and `observedAtUnixMs`;
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

Credentials and cookies are stored in plaintext at:

```text
%APPDATA%\Cyborus\forgejo-cli\data\ui-creds.json   # Windows
~/.local/share/Cyborus/forgejo-cli/data/ui-creds.json  # Linux
```

`fj-ex actions runners` uses the API token stored by the official `fj` CLI at:

```text
%APPDATA%\Cyborus\forgejo-cli\data\keys.json   # Windows
~/.local/share/Cyborus/forgejo-cli/data/keys.json  # Linux
```

`fj-ex token mint nuget` needs both:

```text
fj-ex auth login   # stored username/password for basic auth
fj auth login      # stored API token for Authorization: token ...
```

This is required for automatic re-login. Downloaded logs and artifacts may contain secrets — handle accordingly.

If your account uses two-factor authentication, `fj-ex auth login` prompts for the current passcode after the password step. For noninteractive use, pass `--otp`, `--otp-stdin`, or set `FJ_OTP`. The passcode is not stored; only the resulting UI cookies are stored so later commands can reuse the session until Forgejo expires it.

## License

LGPL-3.0-or-later

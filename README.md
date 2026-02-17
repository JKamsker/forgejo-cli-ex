# fj-ex

`fj-ex` is a small Rust CLI that polyfills missing Forgejo CLI (`fj`) functionality by calling Forgejo **web UI endpoints** (a “UI API”), mainly for Forgejo Actions (full logs, artifacts, cancel/rerun, …).

This is a port of the PowerShell PoC in `C:\Users\Jonas\repos\work\dccx\src\pandocs\Scripts\Forgejo`.

## Security notes

- `fj-ex` stores **plaintext** UI credentials by design (required for auto re-login).
- Cookies are stored alongside credentials to avoid logging in for every request.
- Downloaded logs and artifacts may contain secrets.

Storage location (shared with the PowerShell scripts):

- `%APPDATA%\Cyborus\forgejo-cli\data\ui-creds.json`

## Target resolution (fj-like)

Most commands accept:

- `--host`, `-H` to explicitly target a server (host or base URL)
- `--repo`, `-r` for `owner/name` (or `host/owner/name`)
- `--remote`, `-R` to infer host+repo from a git remote

If `--host` is omitted, `fj-ex` attempts to infer it from the current repo’s git remotes. If that fails, it falls back to:

- `FJ_FALLBACK_HOST`

## Login

Login validates the UI session and persists creds + cookies.

Interactive:

```powershell
fj-ex login --host forge.example.com
```

Via stdin (recommended vs `--password`):

```powershell
("my-password`n") | fj-ex login --host forge.example.com --username my-user --password-stdin
```

Environment fallbacks (no `.env` support):

- `FJ_USER`
- `FJ_PASS`

## Actions (UI endpoints)

List workflows/runs/jobs:

```powershell
fj-ex actions workflows --host forge.example.com --repo owner/name
fj-ex actions runs --host forge.example.com --repo owner/name --limit 20
fj-ex actions jobs --host forge.example.com --repo owner/name --latest
```

Download logs:

```powershell
# Single job to stdout (attempt auto-detected if omitted)
fj-ex actions logs job --host forge.example.com --repo owner/name --run-index 50 --job-index 0

# Whole run to files
fj-ex actions logs run --host forge.example.com --repo owner/name --run-index 50 --out-dir .tmp\\forgejo-logs\\run-50
```

Artifacts:

```powershell
fj-ex actions artifacts list --host forge.example.com --repo owner/name --latest
fj-ex actions artifacts get --host forge.example.com --repo owner/name --run-index 50 --artifact my-artifact --out-file .tmp\\artifact.zip
```

Cancel / rerun (destructive)

These execute immediately by default. Use `--dry-run` to preview.

```powershell
fj-ex actions cancel --host forge.example.com --repo owner/name --run-index 50 --dry-run
fj-ex actions rerun  --host forge.example.com --repo owner/name --run-index 50 --dry-run
```

## Smoke test

Non-destructive validation similar to the PoC:

```powershell
fj-ex smoke-test --host forge.example.com --repo owner/name
```

Write logs somewhere else (avoids creating `.tmp` in the current directory):

```powershell
fj-ex smoke-test --host forge.example.com --repo owner/name --out-dir $env:TEMP\\fj-ex-smoke
```

# Command Reference

All commands support `--host`/`-H`, `--repo`/`-r`, and `--remote`/`-R` for target resolution.
See the main [README](../README.md) for install and quickstart.

---

## auth

```sh
fj-ex auth login --host forge.example.com                      # interactive
fj-ex auth login --host forge.example.com --username my-user --password-stdin
fj-ex auth login --host forge.example.com --username my-user --password-stdin --otp-stdin
fj-ex auth status --host forge.example.com
fj-ex auth list
fj-ex auth show   --host forge.example.com
fj-ex auth logout --host forge.example.com
fj-ex auth clear-cookies --host forge.example.com
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

---

## token mint nuget

Create a NuGet API key using the stored `fj` API token plus the stored `fj-ex` username/password:

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

- Resolves the authenticated user from the stored `fj` API token
- Lists token id, name, last eight characters, and scopes

---

## actions runs

```sh
fj-ex actions runs --repo owner/name --limit 20
fj-ex actions runs --repo owner/name --latest
fj-ex actions runs --repo owner/name --status failure
fj-ex actions runs --repo owner/name --workflow ci.yml
```

---

## actions jobs

```sh
fj-ex actions jobs --repo owner/name --latest
fj-ex actions jobs --repo owner/name --latest --watch
fj-ex actions jobs --repo owner/name --run-index 50 --watch --json
fj-ex actions jobs --repo owner/name --run-index 50 --watch --json-lines
```

---

## actions logs

Download a single job's log to stdout:

```sh
fj-ex actions logs job --repo owner/name --latest --job-index 0
fj-ex actions logs job --repo owner/name --run-index 50 --job-index 0
fj-ex actions logs job --repo owner/name --run-index 50 --job-index 0 --follow --follow-interval 5
```

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
```

---

## actions runners

These commands use the Forgejo REST API and require being authenticated via the official `fj` CLI
(API token stored by `fj`).

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

If you get a "No Forgejo API token found" error, authenticate via `fj`:

```sh
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

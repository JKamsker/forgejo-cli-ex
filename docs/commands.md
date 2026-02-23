# Command Reference

All commands support `--host`/`-H`, `--repo`/`-r`, and `--remote`/`-R` for target resolution.
See the main [README](../README.md) for install and quickstart.

---

## auth

```sh
fj-ex auth login --host forge.example.com                      # interactive
fj-ex auth login --host forge.example.com --username my-user --password-stdin
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

Environment variable fallbacks:

- `FJ_USER`
- `FJ_PASS`

Legacy alias: `fj-ex login --host forge.example.com`

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
```

---

## actions logs

Download a single job's log to stdout:

```sh
fj-ex actions logs job --repo owner/name --latest --job-index 0
fj-ex actions logs job --repo owner/name --run-index 50 --job-index 0
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

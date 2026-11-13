# dispatch-inputs

fj-ex E2E demo repository.

- `dispatch.yml`: `workflow_dispatch` with inputs `greeting` (string) and
  `outcome` (`success`/`failure`). The job echoes the greeting, the length of
  the secret `FJEX_E2E_SECRET` and the value of the variable `FJEX_E2E_VARIABLE`,
  and fails when `outcome=failure`.
- `waiting.yml`: a job with `runs-on: fjex-no-such-runner`, so it stays queued
  and shows up in `fj-ex actions runners jobs --waiting`.

# ci-mixed

fj-ex E2E demo repository: the `mixed` workflow has a `pass` job and a `fail`
job. `fail` exits 1 unless the repository Actions variable
`FJEX_MIXED_SHOULD_PASS` is `true`, so setting the variable and running
`fj-ex actions rerun --failed-only` turns the run green.

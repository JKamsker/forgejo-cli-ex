#!/usr/bin/env bash
# Start one E2E target (server + runner) with the demo repositories seeded.
#
#   tests/e2e/up.sh forgejo-17
#   tests/e2e/up.sh --list          # available targets
#
# Uses the same harness as the Rust E2E tests (examples/e2e_stack.rs).
set -euo pipefail
cd "$(dirname "$0")/../.."

if [[ "${1:-}" == "--list" || "${1:-}" == "targets" ]]; then
  exec cargo run --quiet --locked --example e2e_stack -- targets
fi
if [[ $# -lt 1 ]]; then
  echo "usage: $0 <target> [--work-root DIR]   (see: $0 --list)" >&2
  exit 2
fi
exec cargo run --quiet --locked --example e2e_stack -- up "$@"

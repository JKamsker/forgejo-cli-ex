#!/usr/bin/env bash
# Remove E2E stacks started by up.sh (or kept with FJ_EX_E2E_KEEP=1).
#
#   tests/e2e/down.sh <stack-id> [<stack-id>...]
#   tests/e2e/down.sh --all
#   tests/e2e/down.sh --list        # show running stacks
#
# Only containers labelled `fjex-e2e` are touched.
set -euo pipefail
cd "$(dirname "$0")/../.."

if [[ "${1:-}" == "--list" || "${1:-}" == "list" ]]; then
  exec cargo run --quiet --locked --example e2e_stack -- list
fi
if [[ $# -lt 1 ]]; then
  echo "usage: $0 <stack-id>... | --all | --list" >&2
  exit 2
fi
exec cargo run --quiet --locked --example e2e_stack -- down "$@"

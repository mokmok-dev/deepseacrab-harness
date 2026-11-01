#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

jobs="${JOBS:-4}"
timeout="${TIMEOUT:-300}"
command=(cargo mutants --workspace --jobs "$jobs" --timeout "$timeout" --baseline run)

if command -v systemd-run >/dev/null 2>&1; then
  exec systemd-run --user --scope -p MemoryMax=80% -p MemoryHigh=70% \
    nix develop --command "${command[@]}"
fi

exec nix develop --command "${command[@]}"

#!/usr/bin/env bash
set -euo pipefail
export RUST_ROOT="${RUST_ROOT:-$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)}"
default_csharp_root=/home/sparrow/RiderProjects/AssumptionZero
export CSHARP_ROOT="${CSHARP_ROOT:-$default_csharp_root}"
csharp_root_explicit=0
args=("$@")
for ((i=0; i<${#args[@]}; i++)); do
  if [[ ${args[i]} == --csharp-root ]]; then
    [[ $((i+1)) -lt ${#args[@]} ]] || { echo '--csharp-root needs a value' >&2; exit 1; }
    export CSHARP_ROOT="${args[i+1]}"
    csharp_root_explicit=1
  fi
done
# A stale shell variable must not prevent this machine's default installation working.
if [[ ! -f "$CSHARP_ROOT/scripts/benchmark-common.sh" && $csharp_root_explicit == 0 && -f "$default_csharp_root/scripts/benchmark-common.sh" ]]; then
  export CSHARP_ROOT=$default_csharp_root
fi
[[ -f "$CSHARP_ROOT/scripts/benchmark-common.sh" ]] || {
  printf 'Shared benchmark scripts not found at %s/scripts/benchmark-common.sh\n' "$CSHARP_ROOT" >&2
  exit 1
}
source "$CSHARP_ROOT/scripts/benchmark-common.sh"
az_main rust "$@"

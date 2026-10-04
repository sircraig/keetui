#!/usr/bin/env bash
# Start keetui with the vault in this directory by default.
#
#   ./start.sh                        open ./Database.kdbx
#   ./start.sh other.kdbx             open another vault (offers to create it if missing)
#   ./start.sh --keyfile /path/key    extra flags are passed through
set -euo pipefail
cd "$(dirname "$0")"

db="./Database.kdbx"
# First argument that isn't a flag overrides the database path.
if [[ $# -gt 0 && $1 != -* ]]; then
    db="$1"
    shift
fi

cargo build --release --quiet
exec ./target/release/keetui "$db" "$@"

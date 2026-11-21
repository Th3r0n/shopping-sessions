#!/bin/bash
# The full correctness gauntlet on the native binary. Usage: scripts/check.sh [fuzz cases]
set -e
cd "$(dirname "$0")/.."
D=/c/Users/Joaopc/Documents/buildathon/data
B="$(cygpath -w "$PWD/target/release/shopping-sessions.exe")"
CASES=${1:-600}
cargo build --release 2>&1 | grep -E "^(warning|error)" -A5 || true
cargo test --release 2>&1 | grep -E "test result|FAILED|panicked"
python scripts/run_fixtures.py fixtures -- "$B" | tail -1
SS_SCALAR=1 python scripts/run_fixtures.py fixtures -- "$B" | tail -1
python scripts/fuzz.py "$B" --cases $CASES --seed 11 2>/dev/null | tail -1
SS_SCALAR=1 python scripts/fuzz.py "$B" --cases $CASES --seed 12 2>/dev/null | tail -1
rm -rf "$D/chk-dev" && "$B" "$D/dev" "$D/chk-dev" 2>/dev/null && python scripts/compare.py "$D/chk-dev" "$D/dev-answer" | tail -1
rm -rf "$D/chk-p20" && "$B" "$D/p20" "$D/chk-p20" 2>/dev/null && python scripts/compare.py "$D/chk-p20" "$D/out-p20" | tail -1
rm -rf "$D/chk-p20b" && NCPU=3 SS_PARTS=211 SS_CHUNK=999983 "$B" "$D/p20" "$D/chk-p20b" 2>/dev/null && python scripts/compare.py "$D/chk-p20b" "$D/out-p20" | tail -1

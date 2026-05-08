#!/usr/bin/env bash
# tests/release/packages/_fixture_lib.sh — shared helpers for fixture.sh.
#
# Source from each fixture.sh. Provides:
#   fixture_setup <name>
#       cd to the fixture dir, set PERRY_BIN, npm install if needed.
#       Returns non-zero on install failure.
#
#   fixture_compile_run_diff <name> [entry.ts] [expected.txt]
#       perry compile entry → ./out → run → diff against expected.
#       Prints PASS or FAIL with context. Returns the appropriate exit code
#       so the caller can `exit $?`.
#
#   fixture_skip <name> <reason>
#       Mark this fixture as SKIP. The harness reads the .last-skip
#       sentinel file to count it correctly. Exits 0.
#
# Usage from a fixture.sh:
#   #!/usr/bin/env bash
#   set -uo pipefail
#   cd "$(dirname "$0")"
#   . "$(dirname "$0")/../_fixture_lib.sh"
#   fixture_setup "my-fixture" || exit 1
#   fixture_compile_run_diff "my-fixture"

# Resolve repo root + perry binary based on the lib's own location.
_FIXLIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
_FIXLIB_REPO_ROOT="$(cd "$_FIXLIB_DIR/../../.." && pwd)"

fixture_setup() {
    local name="$1"
    PERRY_BIN="${PERRY_BIN:-$_FIXLIB_REPO_ROOT/target/release/perry}"
    if [[ ! -x "$PERRY_BIN" ]]; then
        echo "FAIL $name — perry not found at $PERRY_BIN"
        return 1
    fi
    if [[ -f package.json && ! -d node_modules ]]; then
        echo "  [npm install] $name..."
        if ! npm install --silent --no-audit --no-fund > install.log 2>&1; then
            echo "FAIL $name — npm install failed"
            sed 's/^/    /' install.log | tail -20
            return 1
        fi
    fi
    return 0
}

fixture_compile_run_diff() {
    local name="$1"
    local entry="${2:-entry.ts}"
    local expected="${3:-expected.txt}"

    echo "  [perry compile] $entry"
    if ! "$PERRY_BIN" "$entry" -o ./out > perry-compile.log 2>&1; then
        echo "FAIL $name — perry compile errored"
        sed 's/^/    /' perry-compile.log | tail -40
        return 1
    fi
    echo "  [./out]"
    if ! ./out > perry-out.txt 2> perry-run.log; then
        echo "FAIL $name — runtime exit non-zero"
        sed 's/^/    /' perry-run.log | tail -40
        echo "    --- stdout (truncated) ---"
        sed 's/^/    /' perry-out.txt | tail -20
        return 1
    fi
    echo "  [diff] $expected vs perry-out.txt"
    if ! diff -u "$expected" perry-out.txt > diff.log; then
        echo "FAIL $name — output diverges"
        sed 's/^/    /' diff.log
        return 1
    fi
    echo "PASS $name"
    return 0
}

fixture_skip() {
    local name="$1"
    local reason="$2"
    echo "SKIP $name — $reason"
    # _harness.sh reads this sentinel to count SKIP separately from PASS
    touch .last-skip
    exit 0
}

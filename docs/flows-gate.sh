#!/usr/bin/env bash
# The per-chunk gate for the flows branch.
#
#   bash docs/flows-gate.sh "-p ns-core" flow
#   bash docs/flows-gate.sh "-p ns-engine -p ns-app" --test flow
#
# Arg 1: the cargo package flags. Remaining args: passed to `cargo test` after the packages.
#
# The branch point is not fmt- or clippy-clean (13 unformatted files, one pre-existing clippy lint in
# ns-core), so this gate judges only the .rs files changed since the branch point: none of them may have
# a fmt diff or a clippy warning. Prints PASS or FAIL as its last line.
set -u
cd "$(dirname "$0")/.." || exit 2
TD=C:/Users/Acemagic/cargo-targets/flows
BASE=ab2c3c6
PKGS=$1
shift

changed=$( { git diff --name-only "$BASE"; git ls-files --others --exclude-standard; } | grep '\.rs$' | sort -u)
echo "changed .rs files:"; echo "$changed" | sed 's/^/  /'
fail=0

# 1. fmt: no diff in a changed file.
fmt=$(cargo fmt --all -- --check 2>&1 | grep '^Diff in' | sed -E 's/^Diff in (.*) at line ([0-9]+).*/\1:\2/' | tr '\\' '/')
for f in $changed; do
  hits=$(echo "$fmt" | grep -F "/$f:" || true)
  if [ -n "$hits" ]; then echo "FMT: $f is not rustfmt-clean:"; echo "$hits" | sed 's/^/  /'; fail=1; fi
done

# 2. clippy: no warning or error located in a changed file, and no compile error anywhere.
clip=$(cargo clippy $PKGS --all-targets --target-dir "$TD" -j 8 --message-format short 2>&1)
if echo "$clip" | grep -qE '^error(\[E[0-9]+\])?:|: error(\[E[0-9]+\])?:'; then
  if echo "$clip" | grep -E ': error' | grep -v 'large size difference between variants' | grep -q .; then
    echo "CLIPPY: compile errors:"; echo "$clip" | grep -E ': error' | head -20; fail=1
  fi
fi
for f in $changed; do
  hits=$(echo "$clip" | tr '\\' '/' | grep -E "^$f:[0-9]+:[0-9]+: (warning|error)" || true)
  if [ -n "$hits" ]; then echo "CLIPPY: $f:"; echo "$hits" | sed 's/^/  /'; fail=1; fi
done

# 3. tests.
out=$(cargo test $PKGS --target-dir "$TD" -j 8 "$@" 2>&1)
code=$?
echo "$out" | grep -E '^test result|FAILED|panicked|^error' | head -40
if [ $code -ne 0 ]; then echo "TESTS: cargo test exited $code"; fail=1; fi

if [ $fail -eq 0 ]; then echo PASS; else echo FAIL; fi
exit $fail
</content>
</invoke>
<invoke name="mcp__ccd_turn__update_plan">
<parameter name="advance">true
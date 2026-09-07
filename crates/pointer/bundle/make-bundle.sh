#!/usr/bin/env bash
# Package `ns-pointer` as a standalone crate for the machine it will drive.
#
# The crate has no path dependencies and no workspace ties, so the source tree
# plus the pinned manifest next to this script is a complete, buildable unit.
# The manifest is a checked-in file, not generated here: see its header for why
# the tokio and mio pins exist and what breaks without them.
#
#   ./crates/pointer/bundle/make-bundle.sh [outdir]
#
# The verification build runs `cargo test --offline` by default, so it proves
# the bundle needs nothing that is not already cached. Set CARGO_FLAGS to an
# empty string to let it resolve against crates.io instead, which is what a
# fresh machine and a changed pin both need:
#
#   CARGO_FLAGS= ./crates/pointer/bundle/make-bundle.sh
#
# Verification is not optional and not a flag. The script extracts what it just
# wrote into a scratch directory and builds it there, because a bundle that has
# only ever been built from the workspace is a hopeful bundle: the workspace
# supplies a lockfile, a resolver context and a set of feature unifications
# that the tarball does not.
#
# Two things are checked, and the split between them is the useful part.
#
# *Resolution* is checked for the real target. `cargo tree --target
# x86_64-pc-windows-gnu` resolves the Windows dependency graph from here
# without compiling any of it, so the regression the pins exist to prevent —
# windows-sys advancing to the `raw-dylib` line and demanding `dlltool` — is
# caught on Linux, at the moment someone runs a stray `cargo add`, instead of
# on the target machine days later. That check is below and it is fatal.
#
# *Compilation* is not, and cannot be. Nothing here builds Windows objects, so
# a green run means the protocol logic is intact, the manifest is
# self-consistent, and the Windows graph resolves to versions known to link.
# It is not evidence that the Windows build succeeds — only building on
# Windows is that.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
crate="$(dirname "$here")"
out="${1:-$crate/../../scratchpad/bundle}"
mkdir -p "$out"
out="$(cd "$out" && pwd)"

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
root="$stage/ns-pointer"
mkdir -p "$root"

cp -R "$crate/src" "$crate/tests" "$crate/examples" "$root/"
cp "$here/Cargo.toml" "$root/Cargo.toml"

# The protocol is the contract between the two ends; the work split says who
# owns which half. Both travel with the code or the code arrives unexplained.
mkdir -p "$root/docs"
for d in pointer-protocol.md pointer-work-split.md 2026-09-05-ns-pointer-reply.md; do
    [ -f "$crate/../../docs/$d" ] && cp "$crate/../../docs/$d" "$root/docs/"
done

tar -czf "$out/ns-pointer-bundle.tar.gz" -C "$stage" ns-pointer
echo "wrote $out/ns-pointer-bundle.tar.gz"

# Build it from the tarball, in a directory that shares nothing with here.
check="$(mktemp -d)"
trap 'rm -rf "$stage" "$check"' EXIT
tar -xzf "$out/ns-pointer-bundle.tar.gz" -C "$check"
echo "--- verifying in $check/ns-pointer ---"
( cd "$check/ns-pointer" && cargo test ${CARGO_FLAGS---offline} )
echo "--- bundle builds and tests from its own tarball ---"

# The pin check. See the manifest header: the lockfile is not a witness here,
# because a unix-only windows-sys 0.61 arrives through signal-hook-registry
# whether or not the pins hold. Resolving for the one target that matters is.
echo "--- resolving the x86_64-pc-windows-gnu graph ---"
tree="$(cd "$check/ns-pointer" && cargo tree --target x86_64-pc-windows-gnu -i windows-sys@0.52.0 2>&1 || true)"
if printf '%s' "$tree" | grep -q '^windows-sys v0.52'; then
    echo "ok: windows-sys 0.52 (prebuilt import libraries, no dlltool)"
else
    echo "FAIL: windows-sys 0.52 is not in the windows-gnu graph." >&2
    echo >&2
    echo "The tokio/mio pins have been resolved away. Unpinned, this graph" >&2
    echo "takes windows-sys 0.61, which links via raw-dylib and needs" >&2
    echo "dlltool.exe — the Windows build dies before reaching any of our" >&2
    echo "code. Restore the '=' pins in crates/pointer/bundle/Cargo.toml." >&2
    echo >&2
    printf '%s\n' "$tree" >&2
    exit 1
fi

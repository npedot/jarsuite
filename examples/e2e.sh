#!/usr/bin/env bash
# End-to-end check of the example suite on this platform (run ./build.sh first):
# assemble the three releases, install 1.0, run both tools, update to 2.0 and make
# sure the refused cases are refused. tool-a capitalizes what it adds.
#
# jarsuite: $JARSUITE, else `jarsuite` on the PATH. Without $JAREXEC, assemble downloads
# the jarexec release named in suite-*.toml.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
jarsuite="${JARSUITE:-jarsuite}"
case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) platform=linux-x64 ;;
    Linux-aarch64) platform=linux-aarch64 ;;
    Darwin-arm64) platform=mac-aarch64 ;;
    Darwin-x86_64) platform=mac-x64 ;;
    MINGW*|MSYS*) platform=windows-x64 ;;
    *) echo "unsupported platform $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac
local_jarexec=()
[[ -n "${JAREXEC:-}" ]] && local_jarexec=(--jarexec "$platform=$JAREXEC")

work="$here/e2e"
rm -rf "$work"
export JARSUITE_ROOT="$work/root" JARSUITE_DATA="$work/data"

step() { echo "== $*"; }
fail() { echo "FAILED: $*" >&2; exit 1; }
# runs a command that must fail with a message containing $1
refused() {
    local expected="$1"; shift
    local output
    if output="$("$@" 2>&1)"; then fail "$* succeeded"; fi
    [[ "$output" == *"$expected"* ]] || fail "$*: expected \"$expected\", got: $output"
    echo "refused as expected: $expected"
}

assemble() {
    "$jarsuite" assemble "$here/$1" -o "$work/pub/$2" --platform "$platform" "${local_jarexec[@]}"
}

step "assemble"
assemble suite-1.toml 1
assemble suite-2.toml 2
refused "must be aligned" assemble suite-mixed.toml 3

step "install 1.0 and run both tools"
"$jarsuite" init "$work/pub/1/suite.toml"
"$jarsuite" update
"$jarsuite" run tool-a add hello
[[ "$("$jarsuite" run tool-b list)" == *Hello* ]] || fail "tool-b does not list the entry"

step "update to 2.0: reads the data written by 1.0"
"$jarsuite" init "$work/pub/2/suite.toml"
"$jarsuite" update
"$jarsuite" run tool-a add again
listed="$("$jarsuite" run tool-b list)"
[[ "$listed" == *Hello* && "$listed" == *Again* ]] || fail "tool-b list after the update: $listed"

step "rollback below the data format"
refused "data format" "$jarsuite" rollback

echo "e2e: ok"

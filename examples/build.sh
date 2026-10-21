#!/usr/bin/env bash
# Builds tool-a and tool-b against commons 1.0.0 (data format 1) and 2.0.0 (data format 2),
# then lays out three ready-to-run folders for this platform:
#   out/aligned-1  both tools on commons 1.0.0
#   out/aligned-2  both tools on commons 2.0.0
#   out/mixed      tool-a on 2.0.0, tool-b on 1.0.0: what a suite launcher must never install
#
# jarexec: $JAREXEC, else `jarexec` on the PATH, else built from ../../jarexec.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
out="$here/out"
tools=(tool-a tool-b)

jarexec="${JAREXEC:-$(command -v jarexec || true)}"
if [[ -z "$jarexec" ]]; then
    repo="$here/../../jarexec"
    [[ -f "$repo/Cargo.toml" ]] || { echo "jarexec not found: set JAREXEC" >&2; exit 1; }
    cargo build --release --quiet --manifest-path "$repo/Cargo.toml"
    target="$(cargo metadata --manifest-path "$repo/Cargo.toml" --format-version 1 --no-deps \
        | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
    jarexec="$target/release/jarexec"
fi
exe=""
[[ "$jarexec" == *.exe ]] && exe=".exe"

rm -rf "$out"
for major in 1 2; do
    dest="$out/aligned-$major"
    echo "== commons $major.0.0, data format $major -> $dest"
    mvn -q -f "$here/pom.xml" -Drevision="$major.0.0" -Ddata.format="$major" clean package
    mkdir -p "$dest"
    for tool in "${tools[@]}"; do
        jar="$here/$tool/target/$tool-1.0.0.jar"
        "$jarexec" lock --deps "$here/$tool/target/jarexec-deps.txt" --jar "$jar" \
            --bundle 'dev.jarsuite.examples:commons' -o "$dest/$tool.lock"
        cp "$jar" "$dest/"
        cp "$jarexec" "$dest/$tool$exe"
    done
done

# Each lock names its own commons JAR in lib/, so the two versions sit side by side.
echo "== mixed: tool-a from aligned-2, tool-b from aligned-1 -> $out/mixed"
mkdir -p "$out/mixed/lib"
for f in tool-a.lock "tool-a$exe" tool-a-1.0.0.jar lib/commons-2.0.0.jar; do cp "$out/aligned-2/$f" "$out/mixed/$f"; done
for f in tool-b.lock "tool-b$exe" tool-b-1.0.0.jar lib/commons-1.0.0.jar; do cp "$out/aligned-1/$f" "$out/mixed/$f"; done

echo "done: $(ls "$out" | tr '\n' ' ')"

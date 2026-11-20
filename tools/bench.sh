#!/bin/sh
# A reproducible evaluation point for zmfuzz.
#
#   tools/bench.sh [story] [games] [turns] [seed]
#
# Always starts fresh, uses one thread, and prints only `key=value` metrics, so
# two runs (before/after a change) can be diffed directly:
#
#   tools/bench.sh > before.txt
#   ...change...
#   tools/bench.sh > after.txt
#   diff before.txt after.txt
#
# Keep the story, game count, turn count and seed identical when comparing.
set -eu

story=${1:-minizork.z3}
games=${2:-400}
turns=${3:-200}
seed=${4:-1}

here=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/zmfuzz-bench.XXXXXX")
trap 'rm -rf "$work"' EXIT

if [ ! -f "$story" ]; then
    story="$here/../folly/encrusted-heart/tests/$story"
fi
story=$(cd "$(dirname "$story")" && pwd)/$(basename "$story")

cargo build --release --manifest-path "$here/Cargo.toml" >/dev/null

start=$(date +%s)
"$here/target/release/zmfuzz" "$story" \
    --fresh --runs "$games" --turns "$turns" --seed "$seed" \
    --threads 1 --no-ui --stats \
    --checkpoint "$work/bench.save" \
    | sed -n '/^== stats ==/,$p'
end=$(date +%s)

echo "elapsed_s=$((end - start))"
echo "story=$(basename "$story")"
echo "games=$games turns=$turns seed=$seed"

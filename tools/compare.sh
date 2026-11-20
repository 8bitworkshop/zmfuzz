#!/bin/sh
# Run zmfuzz and search.js on the same story and compare what they learned.
#
#   tools/compare.sh [story] [seconds] [plays]
#
# The RNGs differ, so the token sets will never match exactly; what we look at is
# overlap (do both find the same content?) and the rate of search events.
set -eu

story=${1:-minizork.z3}
seconds=${2:-15}
plays=${3:-1500}
jszm=${JSZM_DIR:-$HOME/if/jszm}

here=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/zmfuzz-compare.XXXXXX")
trap 'rm -rf "$work"' EXIT

if [ ! -f "$story" ]; then
    story="$here/../folly/encrusted-heart/tests/$story"
fi
story=$(cd "$(dirname "$story")" && pwd)/$(basename "$story")

echo "== zmfuzz: $plays plays of $story"
cargo build --release --manifest-path "$here/Cargo.toml" >/dev/null
(cd "$work" && "$here/target/release/zmfuzz" "$story" --runs "$plays" --turns 200 \
    --seed 1 --checkpoint "$work/zm.save" -v >"$work/zm.log" 2>&1) || true

echo "== search.js: ${seconds}s"
mkdir -p "$work/js"
cp "$jszm/jszm.js" "$jszm/search.js" "$jszm/package.json" "$work/js/"
ln -sfn "$jszm/node_modules" "$work/js/node_modules"
cp "$story" "$work/js/story.z3"
(cd "$work/js" && VERBOSE=1 timeout "$seconds" node search.js story.z3 >"$work/js.log" 2>&1) || true

# "NEWTOKEN <token> <cmd>": the token may contain spaces, so keep everything
# before the last word (the command).
awk '/^NEWTOKEN /{sub(/^NEWTOKEN /,""); sub(/ [^ ]*$/,""); print}' "$work/zm.log" | sort -u >"$work/zm.toks"
awk '/^NEWTOKEN /{sub(/^NEWTOKEN /,""); sub(/ [^ ]*$/,""); print}' "$work/js.log" | sort -u >"$work/js.toks"

report() {
    printf '%-10s %6s %6s %6s %6s %6s %6s\n' "" TOKENS STABLE HACK DIVERGE REDUCE SCORE
    for side in zm js; do
        log="$work/$side.log"
        printf '%-10s %6s %6s %6s %6s %6s %6s\n' "$side" \
            "$(wc -l <"$work/$side.toks" | tr -d ' ')" \
            "$(grep -c '^STABLE' "$log" || true)" \
            "$(grep -c '^HACK' "$log" || true)" \
            "$(grep -c '^DIVERGE' "$log" || true)" \
            "$(grep -c '^REDUCE' "$log" || true)" \
            "$(grep -c '^SCORE' "$log" || true)"
    done
}
report

common=$(comm -12 "$work/zm.toks" "$work/js.toks" | wc -l | tr -d ' ')
total=$(comm -1 -2 --total "$work/zm.toks" "$work/js.toks" 2>/dev/null | wc -l | tr -d ' ' || true)
echo
echo "shared tokens: $common"
echo "only zmfuzz:   $(comm -23 "$work/zm.toks" "$work/js.toks" | wc -l | tr -d ' ')"
echo "only search.js: $(comm -13 "$work/zm.toks" "$work/js.toks" | wc -l | tr -d ' ')"

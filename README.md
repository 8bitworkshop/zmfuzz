# zmfuzz

A shortest-path-to-every-atom fuzzer for Z-machine story files: it plays a game
over and over, records the command sequence that first produced each piece of
output, then aims the next game at whatever it has seen least.

This is a Rust port of `search.js` from [zzo38's JSZM](https://github.com/zzo38/jszm)
(search half only, `jszm.js` is not involved). The VM is
[`encrusted-heart`](../folly/encrusted-heart), so where the JS version only
handles v3, this one also plays v4, v5 and v8.

```
$ cargo run --release -- minizork.z3 --runs 100 --turns 200 -v
zmfuzz: v3 story, 100 games of 200 turns, seed 1
NEWTOKEN You are empty-handed. take mail
REDUCE You are empty-handed. 0 < 99999 (0/0/1)
NEWTOKEN @mv_112_87 open mailbo
...
run 100: plays=100 turns=202 tokens=  268 vocab= 523 stable=  8 turn-limit

goals (cheapest first):
       2.386  @mv_112_158                          first=1 runs=1 succ=1 stable=false score=0
       3.386  @mv_112_22                           first=2 runs=1 succ=1 stable=false score=0
```

## Why a sibling crate

`zmfuzz` is deliberately *not* a member of the `folly` workspace. Building
anything from `folly`'s root resolves `armrest` -> `tflite` -> the `tensorflow`
git submodule, which is slow and unnecessary here. The empty `[workspace]` table
makes this its own root; the only dependency is a path into `encrusted-heart`.

## Usage

```
zmfuzz <story.z3> [options]

  --runs N       games to play (0 = forever, default 1)
  --turns N      turns per game (default 200, as in search.js)
  --seed N       RNG seed (default 1; same seed, same search)
  --top N        how many goal candidates to list (default 20)
  --checkpoint F read/write search state at F (default ./<story>.save)
  --resume       start from the checkpoint instead of a blank search
  --no-events    ignore VM side effects (text tokens only)
  --log          trace every VM instruction on stderr (very noisy)
  -v, --verbose  print NEWTOKEN / REDUCE / STABLE / SCORE / HACK / DIVERGE
```

Progress is measured in **tokens**. A token is either a printed chunk or a VM
state change:

```
@mv_<object>_<parent>      object tree move
@pp_<object>_<property>_1  put_prop
@f_<object>_<flag>_<0|1>   set_attr / clear_attr
```

`@store_*` (global writes) is deliberately absent: jszm logs it and then returns
before recording, so `search.js` never makes those tokens. `--no-events` turns
off the whole `@` family.

## How it works

Per token, `alltokstats` keeps: how many turns it appeared in, how many times it
was the goal (`goalruns`), how many of those games met it (`goalsucc`), the
earliest turn index it showed up at (`first`), the playthrough snapshot that
reaches it (`best`), a stability counter, and the best score seen while it was
the goal.

A game (`play_game`) picks a **goal** at random from the ten most attractive
tokens, then plays up to `maxturns` turns. At each prompt:

1. **Commit** the previous turn: add it to the playthrough, bump the counts, and
   for any token seen earlier than ever before, snapshot the playthrough as that
   token's `best` path (`REDUCE`).
2. If the goal token appeared in that turn, count a success and maybe promote it
   to **stable** (`STABLE`), which takes `5 * (first + 1)` successes.
3. Pick the next command:
   - if the goal has a `best` path, replay or shuffle the commands along it
     (replays are checked: a turn whose tokens differ from the recorded ones is a
     `DIVERGE`, and replay mode is abandoned);
   - otherwise send a random 1-2 word command, half the time drawing on words the
     game has printed that are also in its dictionary.
4. Every `maxturns`-th turn, send `SCORE` first so scoring progress gets recorded.

Priority (lower is more attractive, straight from search.js):

```
first * 1 + ln(1+goalruns) * 4 - ln(1+goalsucc) * 2 + stable * 1000 - maxscore * 100
```

Saturating the run/success counts stops one token from dominating; the score term
is what makes a scored path crowd out everything else.

**VM hacking**: when the chosen goal is stable, with probability
`stablecount/10` the game starts with an object moved somewhere it cannot
normally go (one of the `@mv_` tokens, applied with `Zmachine::mv`), and the
playthrough is prefixed with the shortest known path to that token. This is what
breaks the search out of logjams.

## Checkpoints

`--checkpoint` writes `{numplays, stabletoks, stats[]}` as JSON, with best paths
included, on exit and every 10,000 games. `--resume` reads it back, re-interns
tokens, and rebuilds the goal queue. Unlike the JS version (which uses `flatted`
because JS object graphs are cyclic), best paths are stored as plain nested
arrays, so the file is readable and diffable.

Note that token ids are per-process: a resumed search assigns ids in checkpoint
order, so ids are not comparable across runs. Goals are queued by name, not id,
which is why a resumed search picks the same goals in the same order.

## Parity with search.js

Kept as-is: constants, priority formula, stability thresholds, replay/shuffle
probabilities, divergence detection, hackvm odds, the score-token state machine,
first-turn tokens being excluded from the goal queue, and the `NEWTOKEN` /
`REDUCE` / `STABLE` / `DIVERGE` / `SCORE` / `HACK` event stream (same messages,
so traces can be compared by eye or by `tools/compare.sh`).

Deliberately different:

| search.js | here | why |
| --- | --- | --- |
| `Math.random()` | xorshift64* | V8 seeds from entropy; both are reproducible, but not against each other |
| hangs if the game never reads | `step_with_budget` gives up, run marked `stuck` | a fuzzer should not wedge |
| writes `monkey.z3.save` next to the story | `./monkey.z3.save` | don't scribble in `encrusted-heart/tests` |
| `flatted` checkpoint | JSON | no cyclic graphs to worry about |
| save/restore ask for a file name | in-memory save slot | no interactive input available |
| dictionary in `Map` order | dictionary sorted | `get_dictionary()` sorts; word choice is random either way |

Dropped, because they are dead in `search.js`: `usevmstate` (always false, so the
`state`/`MERGE` machinery never runs), `usecondtok` (false), `turnmods` (the only
test of it is commented out), and the `restarted` hook (nothing to reset that
`newgame` does not already reset).

Token granularity matches jszm: one token per *print opcode chunk*, not per word,
so "You are empty-handed." is a token and "You" is not. `zmfuzz::ui::TokenUI`
exists for exactly this reason -- `BaseUI` coalesces chunks, which would blur
tokens together.

## Tests

```
cargo test                          # determinism, checkpoint round trip, v3/v5 runs
tools/compare.sh minizork.z3 15     # side-by-side with search.js (needs node + jszm)
```

`encrusted-heart` has its own interpreter conformance suite (`tests/*.regtest`);
`zmfuzz` only checks the search layer.

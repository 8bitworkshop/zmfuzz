# zmfuzz

A shortest-path-to-every-atom fuzzer for Z-machine story files: it plays a game
over and over, records the command sequence that first produced each piece of
output, then aims the next game at the cheapest goal according to a priority
function (see [How it works](#how-it-works)).

This is a Rust port of `search.js` from
[sehugg's fork of zzo38's JSZM](https://github.com/sehugg/jszm)
(search half only, `jszm.js` is not involved.
For more information see the [blog post](https://8bitworkshop.com/docs/posts/2020/fuzzing-the-z-machine.html).
)

The VM is
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

## Usage

```
zmfuzz <story.z3> [options]

  --runs N       games to play (0 = forever, default 1)
  --turns N      turns per game (default 200, as in search.js)
  --seed N       RNG seed (default 1; same seed, same search)
  --top N        how many goal candidates to list (default 20)
  --threads N    worker threads (default 1)
  --batch N      games per worker before reporting/merging (default 10)
  --sync N       re-share merged knowledge every N games (0 = end only, default 1000)
  --checkpoint F read/write search state at F (default ./<story file name>.save)
  --fresh        ignore an existing checkpoint and start a blank search
  --no-events    ignore VM side effects (text tokens only)
  --stats        print aggregate search metrics (implies quiet run lines)
  --ui           live full-screen display (default: on when stdout is a TTY)
  --no-ui        plain per-run output even on a terminal
  --screen S     start on pane S: now tree map tokens workers log (or 1-6)
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

The ids are the token's *identity*; everything you see is labelled with object
names read out of the story's object table:

```
@mv_112_134   thief → Maze#134
@f_28_13_1    Behind House flag13=1
@pp_30_7_1    you prop7=1
```

Names are suffixed with the object number only when they are ambiguous —
Mini-Zork has six rooms called `Maze`, and a name without its number would merge
six distinct moves into one goal. Attribute and property numbers have no names
anywhere in a story file, so they stay numeric.

Moves are split into player-driven and ambient. The player object is read from
the object table (`you`, `yourself`, ...) or inferred from the first room change,
and a move by any other object is *ambient*: NPC movement that no command can
control. Ambient tokens stay in the statistics and the display, but never enter
the goal queue or `hackvm`. Moves *into* the player's inventory are kept, so
`take egg` remains a real goal. This is what stops surprising amounts of time
going into "thief → Maze#57".

A verified replay credits the goals it reproduced, not just the chosen one.
Every token first discovered on an intentional replay turn (capped at a few per
game) gets a success too, so one good run can retire a prefix's worth of goals.

Commands that actually changed something — a state token, a room change, or
text never seen before — are mined as word templates. A later turn can take one
of those commands and mutate a single slot, which is much more likely to parse
than two random dictionary words.

## Architecture

There is one search core and two drivers. Everything that crosses a process or
thread boundary is a `Checkpoint`; everything that plays a game is a `Search`.

| module | job |
| --- | --- |
| `src/main.rs` | argument parsing, story load, sequential driver, final report, `--stats` |
| `src/search.rs` | the search core: goal selection, priority, command generation, replay/`hackvm`, tokenizing, per-turn commit |
| `src/parallel.rs` | worker threads and the coordinator; batches are exchanged as `Checkpoint`s |
| `src/checkpoint.rs` | JSON persistence plus `load` / `merge` / `merge_delta` |
| `src/stat.rs` | `Stat`, `Playthrough`, `Turn`, and `Score` (the goal-queue ordering key) |
| `src/token.rs` | token interner: `TokenId` -> `Rc<str>` |
| `src/labels.rs` | object table -> readable names, player-object detection |
| `src/ui.rs` | `TokenUI`, the one-token-per-print-chunk UI |
| `src/tui.rs` | crossterm live display (panes, keys, worker rows) |
| `src/rng.rs` | xorshift64* |

The data flow for one game:

```text
main
  -> Search::new(story, seed, turns)
  -> load_checkpoint (default; --fresh skips it)
  -> run_sequential(search) or parallel::run(search, workers)
       search.play_game()
         newgame()            pick a goal, reset per-game state, stretch budget
         machine()            fresh Zmachine + TokenUI + per-game RNG seed
         run_to_input()       step the VM until it asks for a line
         loop
           committurn()       fold the previous turn into Stat / tokfreq / best
           choose_command()   replay a known path, else template/random words
           handle_input(cmd)
           run_to_input()
         endgame()            demote an unmet goal; checkpoint every 10k games
```

Output arrives by two channels, both drained once per turn in `collect_turn`:

- `TokenUI::drain_prints()` yields one string per print opcode chunk. `logprint`
  turns it into a text token (truncated to `wordtoklen`) and also collects
  dictionary words the turn made usable as commands.
- `Zmachine::drain_events()` yields typed `VmEvent`s (`Move`, `Store`,
  `PutProp`, `GetProp`, `Flag`, `Random`). `apply_event` turns the recorded ones
  into `@mv_` / `@pp_` / `@f_` tokens. `Store` is logged only when
  `record_stores` is on, and `GetProp` / `Random` are ignored, matching `search.js`.

Knowledge lives in `Search`:

- `tokens`: the interner. The raw string is the identity; labels are display
  only and are never fed back into the search.
- `stats: HashMap<TokenId, Stat>`: one record per token, the `alltokstats` of
  the JS version.
- `tokfreq: BTreeSet<Score>`: the goal queue. `Score` orders by priority value
  first and token name second, so the queue is stable across processes even
  though ids are not.
- `stabletoks` / `stableorder`: the stable set and its promotion order.
- `vmtoks`: stable `@mv_` tokens that are not ambient, used by `hackvm`.
- `vocab` / `vocabdict`: the story dictionary (sorted by `get_dictionary`).
- `cmdtemplates`: commands that produced a useful turn, in memory only.

`Stat` is the per-token record. `count` is how many turns contained the token;
`goalruns` / `goalsucc` are attempts and verified successes; `first` is the
earliest turn index it appeared at; `best` is a snapshot of the playthrough up
to `first`; `stablecount` drives promotion; `maxscore` and additive `goalruns`
drive the score bonus (`score_tries` is telemetry); `cmds` are the commands that
first produced it.

## How it works

Per token, `Stat` keeps: how many turns it appeared in, how many times it was
the goal (`goalruns`), how many of those games met it (`goalsucc`), the earliest
turn index it showed up at (`first`), the playthrough snapshot that reaches it
(`best`), a stability counter, and the best score seen while it was the goal.

A game (`play_game`) picks a **goal** at random from the ten most attractive
tokens, then plays up to `maxturns` turns. One game in five (`prob_explore`)
instead draws from a pool of the least-tried tokens outside that top ten, so a
few hard goals cannot monopolise the queue. At each prompt:

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

Priority (lower is more attractive):

```
first * 1 + ln(1+goalruns) * 4 - ln(1+goalsucc) * 2
  + fails * (fails / (goalruns+1)) * 10
  + stable * 1000
  - min(150, maxscore * 15 / (1 + goalruns / 5))
```

where `fails = goalruns - goalsucc`. The failure term is weighted by the failure
*rate*, so a goal that occasionally succeeds is not punished like one that never
does. The score decay rides on `goalruns`, which is additive across workers, and
the bonus is capped at 150 so a single lucky scored run cannot pin the queue.
`score_tries` is still recorded as telemetry, but it is not part of the
priority. See [`PLATEAU-DIAGNOSIS.md`](PLATEAU-DIAGNOSIS.md) for the measurements
that led to this formula.

A success updates the goal's priority immediately, not only when it becomes
stable, so the queue reflects `goalsucc` as soon as it changes.

**Deep goals**: the linear `first` term in the priority makes a token first seen
at turn 300 unattractive, so a known-but-unreproduced path can otherwise sit in
the queue forever. Three story-agnostic mechanisms counter that:

- 8% of games (`prob_frontier`) target the deepest non-stable token that has a
  known `best` path, including ones that have succeeded once or twice but are
  still short of their threshold;
- if the chosen goal's `first` (or path length) is beyond `--turns`, that game
  gets a stretched budget, up to 4x the normal turn limit, so the route is
  reachable;
- a `best` path longer than 12 turns is replayed as an ordered sequence, not by
  retrying only the command that originally produced the token. A path found by
  a random game is still a path until it diverges, which still abandons the
  replay for that game.

**VM hacking**: when the chosen goal is stable, with probability
`stablecount/10` the game starts with an object moved somewhere it cannot
normally go (one of the `@mv_` tokens, applied with `Zmachine::mv`), and the
playthrough is prefixed with the shortest known path to that token. This is what
breaks the search out of logjams.

## Threads

`--threads N` runs N independent searches. Each worker owns its own `Search`
(the state holds `Rc<str>` token names, so it is deliberately not shared) and
they exchange only plain checkpoints.

- Each worker is seeded from `--seed` plus its worker number; worker 0 keeps the
  original seed, so `--threads 1` is the old single-threaded, reproducible run.
- Workers play `--batch` games at a time and report back. Workers keep their
  own state between batches; only a sync reloads a master snapshot. Each batch
  reports `count` / `goalruns` / `goalsucc` / `stablecount` as *deltas* from
  that worker's baseline, and the coordinator adds them. Every game belongs to
  exactly one worker, so those merged counters are real global totals rather
  than a max over workers. `stablecount` is signed because a missed goal
  subtracts two. The coordinator also promotes (or demotes) from the *summed*
  `stablecount`, so several workers that are each one success short still cross
  the threshold together. Paths, `first`, scores and the stable set merge
  best-of-either.
- Every `--sync` games the merged snapshot is handed back to workers, so what
  one thread discovers steers the others. `--sync 0` makes the workers fully
  independent until the final merge.
- Only the coordinator writes the checkpoint, every 10,000 games and on exit.
  Parallel runs are not bit-for-bit reproducible with a sequential run; use one
  thread when comparing traces.

## Live display

On a terminal, `zmfuzz` takes over the screen for a live view. A pinned header
keeps the answers that must never scroll away — games played and rate, tokens,
vocab, stable count, top score, goal queue size — above a tab strip and one
switchable pane. The pane is what you flip between:

| pane | shows |
| --- | --- |
| `now` | the featured worker: turn bar, room, score, goal, current command, the route this game has taken, and the newest events |
| `tree` | the command trie: the cheapest goals' best known paths merged by shared prefix (`★` stable, `●` on this game's route) |
| `map` | room graph and object contents (placeholder until rooms are collected) |
| `tokens` | the goal queue with runs/succ/first/count, then stable tokens in promotion order |
| `workers` | one row per worker, including room and score |
| `log` | the event stream, repeats collapsed into `×N` |

Keys: `1`-`6` select a pane, `Tab`/`Shift-Tab` cycle, `↑`/`↓`/`PgUp`/`PgDn`/
`Home`/`End` scroll it (each pane remembers its offset), `w` cycles the featured
worker, `?` toggles the help overlay, `q`/`Esc`/Ctrl-C stop the search. Room and
score come from the VM status bar, which the interpreter redraws on every `read`,
so they cost nothing extra.

It uses the alternate screen and raw input via `crossterm`; the terminal is
restored on exit. `--no-ui` keeps the plain per-run lines, and `--ui` forces the
display on even when stdout is redirected (useful for capturing frames).
`--screen S` picks the starting pane; with `--no-ui` the same flag prints one
plain-text frame at the end instead, which is handy in logs and CI. `-v` only
affects the plain per-run output. `--log` instruction traces go to stderr and
are not useful with the display.

## Checkpoints

`--checkpoint` writes `{format_version, numplays, stabletoks, stats[], run_meta}`
as JSON, with best paths included, on exit and every 10,000 games. The file is
ignored by git (`.gitignore` has `*.z3.save`); delete it or pass `--fresh` for a
blank search. `run_meta` records the thread count, sync interval, code version
and play count, so a stuck blob can be explained after the fact. `score_tries`
is kept for telemetry; score decay uses `goalruns`, which survives a merge.
Checkpoints written before `stablecount` became additive (no `format_version`, or
version 1) are repaired on load: `stablecount` is reconstructed from
`3 * goalsucc - 2 * goalruns` and the stable set is re-derived. The file is
rewritten at the current `format_version` on exit. If the
file exists it is loaded back at startup — tokens are re-interned
and the goal queue rebuilt — so knowledge accumulates across invocations without
retyping `--resume`. The random stream
is reseeded at `seed + numplays`, so each invocation explores new games instead
of replaying the ones the checkpoint already recorded; a given checkpoint plus
`--seed` still reproduces a run exactly. `--fresh` ignores
an existing file and starts blank (it will still be overwritten on exit).
Unlike the JS version (which uses `flatted`
because JS object graphs are cyclic), best paths are stored as plain nested
arrays, so the file is readable and diffable.

Note that token ids are per-process: a resumed search assigns ids in checkpoint
order, so ids are not comparable across runs. Goals are queued by name, not id,
which is why a resumed search picks the same goals in the same order.

## Evaluation

`--stats` prints a machine-friendly block after a run (and implies the noisy
per-run lines are suppressed): new tokens per 1,000 turns, useful and
state-changing command rates, state changes per turn, replay divergence,
average path length, goal-queue health (negative priorities, never-targeted
tokens, top-ten/exploration/frontier shares), score-tries and score-bonus
medians/maxima, the `goalsucc/stablecount` ratio (near the thread count means
stability is being max-merged), and checkpoint size.
`tools/bench.sh` is a reproducible wrapper — fresh checkpoint, one thread, fixed seed — so an A/B can
be a plain `diff`:

```
tools/bench.sh minizork.z3 400 200 1 > before.txt
# ...make a change...
tools/bench.sh minizork.z3 400 200 1 > after.txt
diff before.txt after.txt
```

## Plateau fixes

The 72k-play Mini-Zork checkpoint used to be stuck: almost every stable token was
first seen on turn 0-2 and the top ten goals were the same already-scored
cellar/painting cluster. Three changes address it: `stablecount` is now summed
across workers (and the coordinator promotes from the summed value), score decay
uses additive `goalruns` instead of min-merged `score_tries`, and the score bonus
is rescaled (`scoremult` 15) and capped at 150. Old checkpoints are repaired on
load, and `--stats` reports the diagnostics that make a recurrence visible. The
measurements and root-cause analysis are in
[`PLATEAU-DIAGNOSIS.md`](PLATEAU-DIAGNOSIS.md).

## Parity with search.js

Kept as-is: stability thresholds, replay/shuffle probabilities, divergence
detection, hackvm odds, the score-token state machine, first-turn tokens being
excluded from the goal queue, and the `NEWTOKEN` / `REDUCE` / `STABLE` /
`DIVERGE` / `SCORE` / `HACK` event stream (same messages, so traces can be
compared by eye or by `tools/compare.sh`).

Deliberately different:

| search.js | here | why |
| --- | --- | --- |
| fixed priority formula | failure penalty + capped, goalrun-decayed score + 20% exploration | ten score-6 goals could otherwise pin the queue forever |
| every `@mv_*` is a goal | player moves and inventory pickups only; NPC moves are ambient | chasing the thief is not actionable |
| one goal credited per run | verified replays also credit discoveries on their prefix | one run validates many goals |
| random dictionary words | mined command templates, one slot mutated | fewer parser errors per turn |
| `Math.random()` | xorshift64* | V8 seeds from entropy; both are reproducible, but not against each other |
| hangs if the game never reads | `step_with_budget` gives up, run marked `stuck` | a fuzzer should not wedge |
| writes `monkey.z3.save` next to the story | `./<story file name>.save` in the working directory | don't scribble in `encrusted-heart/tests` |
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
cargo test                          # determinism, checkpoint round trip, merge, score decay, v3/v5 runs
tools/compare.sh minizork.z3 15     # side-by-side with search.js (needs node + jszm)
```

`encrusted-heart` has its own interpreter conformance suite (`tests/*.regtest`);
`zmfuzz` only checks the search layer. The merge tests cover both idempotent
`merge` and the additive `merge_delta` used by the threaded driver, including
stable promotion from the summed counter and legacy-checkpoint repair.

# License
MIT; see `LICENSE`. `encrusted-heart` is linked as a path dependency and is
itself MIT (Sterling DeMille, Ben Kirwin) — see `../folly/encrusted-heart/LICENSE`.

## Why a sibling crate

`zmfuzz` is deliberately *not* a member of the `folly` workspace. Building
anything from `folly`'s root resolves `armrest` -> `tflite` -> the `tensorflow`
git submodule, which is slow and unnecessary here. The empty `[workspace]` table
makes this its own root; the only *path* dependency is `encrusted-heart`
(`crossterm` and `serde` come from crates.io).


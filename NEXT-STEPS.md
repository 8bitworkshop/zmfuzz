# zmfuzz — state and next steps

Last touched: end of the session in which Phase 0/1/2 landed.

## Handoff: this directory is still in /tmp

```
mv /tmp/pi-agent/zmfuzz ~/if/zmfuzz
cd ~/if/zmfuzz
cargo build --release
./target/release/zmfuzz ../folly/encrusted-heart/tests/minizork.z3 --runs 100 --turns 200
```

The `encrusted-heart = { path = "../folly/encrusted-heart" }` dep is relative and
still correct at that location. The empty `[workspace]` table in `Cargo.toml` is
load-bearing — do not remove it, or cargo will pull in folly's
armrest → tflite → tensorflow graph.

Nothing here is tracked by git yet; `git init` if it is going to live in `~/if`.

## What exists

`~/if/folly/encrusted-heart` (modified, uncommitted):

- `src/traits.rs` — `VmEvent` (`Move`/`Store`/`PutProp`/`GetProp`/`Flag`/`Random`).
- `src/options.rs` — `Options::record_events` (default **false**).
- `src/zmachine.rs` — `emit()` at the six opcode sites; `events()`/`drain_events()`;
  `step_with_budget()`; `pc()`, `version()`, `memory()`, `memory_mut()`,
  `restart()`, `mv()`, `object_count()`.
- `tests/events.rs` — 3 tests. `README.md` — instrumentation section.

`/tmp/pi-agent/zmfuzz` (~2,100 lines, only dep is encrusted-heart + serde):

| file | what |
| --- | --- |
| `src/search.rs` | the `GameRunner` port: goals, priority, replay/shuffle, hackvm, committurn |
| `src/stat.rs` | `Stat`, `Playthrough`, `Turn`, `Score` (ordered by value then name) |
| `src/checkpoint.rs` | JSON save/load, tokens as strings |
| `src/ui.rs` | `TokenUI`, one string per print opcode |
| `src/rng.rs` | xorshift64* |
| `src/token.rs` | interner |
| `tools/compare.sh` | side-by-side with `search.js` |
| `tests/search.rs` | determinism, checkpoint round-trip, czech/minizork smoke |

The old phase-0 `src/runner.rs` (plain playthrough loop) has been deleted; its
content was absorbed by `search.rs` + `ui.rs` + `rng.rs`.

## Verified

- `cargo test` in zmfuzz: 6 tests pass (2 unit + 4 integration).
- `cargo test` in encrusted-heart: 3 event tests pass.
- Interpreter regtests: 7/7 (`czech` z3/z4/z5/z8, `praxix`, `curses`, `minizork`).
- Same seed → byte-identical output (`diff` on 20 plays × 200 turns).
- Checkpoint round-trip: same token set, same stable set, same goal queue order
  (this is why `Score` breaks ties by token *name*: ids are per-process).
- Release speed: ~120 plays/s on minizork v3 (search.js: ~154/s).
- Long run (6000 plays, seed 2): 374→430 tokens, ~100 STABLE, 24 HACK, maxscore 10.

### Replay parity harness (new, works)

`zmfuzz --replay commands.txt` replays a fixed command list; `--replay` output
lines are formatted to match a patched `search.js` (`DUMPTURNS=1`). Same story,
same commands, so differences are VM/tokenizer differences.

minizork, first play, 201 turns: **198/201 identical**. The two differences:

- turn 193: JS printed `[There seems to be a noun miss…` where this printed
  `You can't see any … here!` — i.e. the *parser* responded differently
  (noun-missing vs can't-see), before any random element.
- turn 197: same text both sides, but `@mv_112_162` vs `@mv_112_161` — object id
  off by one, almost certainly fallout from 193.

These are the first real VM-behaviour deltas found against jszm and are worth a
look (see below), but note the fuzzer's own stats never depended on them.

## Known deltas vs search.js

Deliberate, documented in README: RNG (`Math.random` vs xorshift64*), hangs
(`step_with_budget`), checkpoint path and format, filesystem save/restore,
sorted dictionary. Dropped as dead in the JS: `usevmstate`/`MERGE`,
`usecondtok`, `turnmods`, `restarted`.

Not yet explained: in `tools/compare.sh minizork.z3 15 1500`, HACK fired 0 times
here vs 313 in search.js. The chain is understood — JS reaches `maxscore` 10 on
many goals, which cancels the +1000 stable penalty so stable goals enter the top
10, and hackvm needs exactly that — but not why its walks score 10 far more
often. Since a seed-2 6000-play run did reach score 10 and 24 HACKs, it looks
like a rate difference rather than a logic bug. Worth one focused session.

## Next steps, in order

1. **Chase the 2 replay differences** (turns 193/197). Reproduce with the smallest
   command list that bisects to turn 193. Likely suspects: dictionary flag/branch
   handling for `noun miss` vs `can't see`, or object 161/162 addressing
   (v3 9-byte entries: parent/sibling/child at +4/+5/+6 — check
   `get_total_object_count` and the sibling walk in `remove_obj`, which recurses
   unbounded on a corrupted tree).
2. **Explain the score-rate gap** (step: instrument both sides to dump
   `(play, commands, curscore)` and compare the first play that reaches 10). If it
   is genuinely the walk, decide whether to keep strict parity or take the better
   variant and document it.
3. **Run the parity harness on every story**: `advent.z3`, `curses.z3`,
   `praxix.z5`, and a v5 with real input; make it a script
   (`tools/replay-parity.sh`) that patches search.js, dumps turns, replays, and
   prints a diff summary — this is the regression test that matters.
4. **`curses.z3` smoke**: it quits after 6-9 turns because `die` is a verb, so it
   needs a longer turn budget or a `--ignore` list to be useful as a target.
5. **Choose where `search.js` lives.** There is an untracked copy in
   `~/if/folly/encrusted-heart/search.js` from the exploration phase. Either
   delete it (it belongs in `~/if/jszm`) or `git add` it with a note; do not leave
   it ambiguous.
6. **Decide on the encrusted-heart changes**: commit the four files + the new
   test, or keep them as an uncommitted patch. The API additions are additive
   except `impl<ZUI: UI>` on the main impl block, which is technically a breaking
   change for anyone holding a `Zmachine<NonUI>`.
7. **Optional**: make `--checkpoint` write only on exit *and* every 10k plays
   (currently both), and prune `best` paths for very old tokens — the JSON for
   minizork already reaches ~1.3 MB after 20 plays, and it grows with the number
   of tokens × path length.
8. **Optional**: `folly` integration — the VM hook is ready, but folly itself
   still cannot build here (armrest/tensorflow). Not this project's job.

## Open questions

- Is byte-level behavioural parity with jszm a goal, or is "same heuristics,
  better VM" enough? The two replay mismatches only matter if the former.
- Should `zmfuzz` learn multiple stories into one checkpoint (shared VM-state
  tokens like `@mv_112_87` are story-specific and would cross-contaminate)?

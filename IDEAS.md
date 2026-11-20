# zmfuzz — ideas for next steps

Companion to `NEXT-STEPS.md` (which still has the older handoff/parity notes).
This file is the search-efficiency roadmap: what is done, what is left, and how
I would sequence it.

## Where things stand

Recent changes:

- Goal priority now has a failure-rate penalty and a decaying `maxscore` bonus
  (`score_tries`), so a lucky score-6 run cannot pin the queue forever.
- 20% of games explore a least-tried goal outside the top ten
  (`prob_explore`).
- Parallel workers keep state between batches and report counter *deltas*; the
  coordinator adds them. `sum(goalruns)` now matches games played.
- Checkpoints require `score_tries`; legacy checkpoints are no longer supported.
- The parallel coordinator reads the object table at startup, so live token and
  tree panes show names rather than raw `@mv_...` ids.

Baseline measurements from the current checkpoint:

| run | tokens | stable | notes |
| --- | ---: | ---: | --- |
| 1,000 resumed games, old heuristics | 481 | 33 | top 10 goals stayed negative-score |
| 1,000 resumed games, new heuristics | 482 | 55 | +22 stable |
| first 100 games, old | — | — | exactly 10 goals targeted |
| first 100 games, new | — | — | 62 distinct goals targeted |
| 400 fresh games, 1 thread | 340 | 6 | `sum(goalruns)=399` |
| 400 fresh games, 4 threads, no sync | 325 | 0 | `sum(goalruns)=396` (4 first-game blanks) |

The search is no longer queue-starved, but it still spends most turns on random
dictionary-word commands. That is the next large inefficiency.

## Prioritized roadmap

### 0. Build an evaluation harness first

Every idea below needs a cheap way to tell whether it helped. Add a `--stats`
mode (or a small `tools/bench.sh`) that reports, for the same seed/checkpoint:

- new tokens per 1,000 turns
- new stable tokens per 1,000 games
- state-changing commands per turn
- useful-command rate: commands that produced a token, move, flag/prop change,
  score, or room change
- goal-queue health: number of negative-priority goals, share of games going to
  the top 10, number of never-targeted tokens
- average path length and replay divergence rate
- checkpoint size and save/load time

Use `--no-ui` and one thread for reproducible A/B runs. This is the prerequisite
for everything else.

### 1. Separate actionable goals from ambient world changes

The search currently treats every `@mv_*` token as a goal, including NPC moves.
In Mini-Zork `@mv_112_*` is the thief and `@mv_30_*` is the player. Chasing
“thief → Maze#57” is wasted: our commands do not control the thief's route.
Replaying a path cannot reliably cause the move; if the thief is scripted it
happens anyway, and if it is random we are only sampling it.

This is not a seed problem. `machine()` already draws a new VM seed every game:

```rust
let seed = self.rng.next_u64();
options.rand_seed = [
    seed as u32,
    (seed >> 32) as u32,
    0x5EED_1234,
    !(seed as u32),
];
```

So every game gets a fresh VM RNG. If the thief still goes to the same room
after three turns, it is either scripted or the RNG path is not being exercised.
Either way, the issue is that ambient state changes are not *actionable*.

Plan:

- Classify tokens: `PlayerMove`, `OtherMove`, `ItemMove`, `Flag`/`Prop`,
  `Text`, `Score`.
- Learn the player object: the `@mv_*_<room>` object whose destination matches
  the status-bar room when the room changes. Fall back to the most frequent
  actor, then name heuristics (`you`, `yourself`, `adventurer`).
- Exclude `OtherMove` tokens from the goal queue and exploration pool. Keep
  them in the stats/display, and optionally credit them when they occur while
  pursuing something else.
- Distinguish player-driven item moves from NPC-driven ones where possible.
- Track replay divergence per goal: if a path rarely reproduces, mark it
  stochastic/ambient and demote or shelve it.
- Keep per-game seed variation for reproducible sampling, but do not expect it
  to make an uncontrollable goal controllable. Optionally derive each machine
  seed from `hash(base_seed, play_index, goal_id)` so every goal gets deliberate
  diversity instead of sharing one global stream.

Quick determinism check: log `VmEvent::Random` (currently ignored in
`apply_event`) and replay the same command list under several seeds. That tells
us whether the thief is scripted or RNG-driven, but the classification fix is
worth doing either way.

### 2. Multi-goal credit and token bundles

Cheap, high impact, and independent of command generation.

Current `committurn`/`metgoal` credit only the chosen goal. But a successful
replay that reaches token A at turn 20 also demonstrably reaches every goal token
on the path prefix. One run can validate many goals.

Ideas:

- When a replayed path is verified (no `DIVERGE`) and reaches its goal, credit
  `goalsucc` / `stablecount` for other goal tokens whose `first` is at or before
  the reached turn and whose token appears on that path.
- Be conservative: only credit tokens on the intentionally replayed prefix, not
  everything that happened during random exploration.
- Group co-produced tokens into a turn *bundle* (room text + `@mv_*` + flag
  changes). A bundle is one observation; one success retires all its members.
- Alternatively, choose bundles as goals directly instead of individual tokens.

Watch out for inflating easy parser-response tokens. Cap credit per game or
require the token to be on the recorded `best` path.

### 3. Command templates and mutation (stage 1 of the command model)

`getrandomcmd` currently picks 1–2 dictionary words, half from recent output.
Most turns are parser errors. Before building a full contextual model, mine the
commands that actually worked.

- Parse commands into `(verb, noun, prep, noun)` slots. If the dictionary exposes
  verb/noun flags, use them; otherwise use position and known command history.
- Record which commands produced a new token, a move, a flag/prop change, or a
  score.
- Generate commands by sampling templates from successful commands, mutating
  one slot, and falling back to random words.
- Bias nouns toward names visible in the room, inventory, or recent output.

This should raise useful commands per turn without needing a room/object model.
It is also the data-collection layer the fuller model needs.

### 4. Context-conditioned command model / inference chains

This is the idea from the last discussion. It is worth doing, but stage it.

#### 4a. Collect context

The search currently keeps only room/score strings for display and discards the
object tree. To condition commands on context we need, per turn:

- current room id/name
- player object and inventory
- objects in the current room
- recently seen objects / adjacent rooms
- last command and its outcome

`Labels` already parses folly's object-tree `Display` output. Either extend that
parser into a real compact tree, or add accessors to `encrusted-heart` (the
better long-term fix). The map pane is blocked on the same data.

#### 4b. Backoff model

Count successful commands by context:

```text
P(command | context) ~ global + room + visible-object + inventory + adjacency
```

Use additive smoothing and prune low-count features. Key questions:

- `P(verb | room)` — e.g. `open`/`unlock` near doors, `take` near objects.
- `P(noun | verb, room/objects)` — e.g. `egg` in the nest room.
- `P(template | recent outcome)` — after "You can't see any X here", try `take`
  or `examine` with a visible object.

Back off to global counts when a context is sparse. Keep an exploration share so
the model does not collapse onto the same few commands.

#### 4c. Inference chains / preconditions

Once commands have observed effects, learn a small dependency graph:

- `take egg` -> egg moves to inventory
- `open egg` -> requires egg in inventory; produces `jeweled egg` reveal
- `unlock grating` -> requires key; `open grating` -> room transition
- `tie rope` -> requires rope and a fixture in the room

This can be used as a prior for command ordering rather than a full planner at
first. A real planner (STRIPS-like) is a later project.

Risks: sparse data, state aliasing, overfitting parser noise, checkpoint bloat.
Start with in-memory top-K tables, then persist only if it pays off.

### 5. Save-state tree search

The biggest structural change. `encrusted-heart` supports in-memory save/restore;
use it to snapshot VM state per turn.

- Store `(memory, globals, stack, PC, RNG state, ...)` at each trie node.
- Replay a path by restoring the deepest snapshot instead of re-running every
  command from turn 0.
- Branch from a node with a new command without replaying the prefix.
- This also removes replay divergence and makes nondeterministic games easier to
  search.

Needs a proper full-state API in the VM. Watch memory usage: a snapshot per turn
for many goals is large; use prefix sharing, deltas, or snapshot only branch
points. Use the eval harness to compare tokens/second and turns-to-discovery.

### 6. Early exit and novelty budget

Currently a game always runs to `maxturns`, even after the goal is met or a
replay diverges early.

- If a replay diverges, abandon that goal instead of spending the rest of the
  game on random commands.
- If the goal is met and no new token has appeared for K turns, end the game and
  start a new goal.
- Track new tokens per game; if a run is producing only known text, cut it short.
- Keep the score turn (`SCORE`) behavior intact.

Tune K with the eval harness so late-game random detours still get a chance.

### 7. Search-heuristic tuning

The new priority formula is better, but some old constants still hurt.

- `stablethresh = 5 * (first + 1)`: deep tokens need hundreds of successes.
  Consider a cap, e.g. `min(5*(first+1), 50)`, or base the threshold on path
  length/success rate.
- `first * 1` strongly penalizes late-game tokens. Consider `ln(1+first)` or
  normalizing by the longest known path.
- Score policy: score goals are still special. Consider a separate score mode
  (chase for N games, then release) instead of a continuous bonus.
- Tune `prob_explore`, `w_fail`, and the failure-rate exponent with the harness.
- Consider weighting token classes differently: room descriptions and `@mv_`
  state changes are progress; generic parser responses are cheap noise.

### 8. Goal representation beyond single tokens

Tokens are a proxy for progress. Alternatives:

- **Turn signatures / bundles**: the set of tokens produced by one command.
- **State fingerprints**: `(room, inventory, object locations, flags)` hash.
  Goal = new state, not new text.
- **Observation goals**: room description + visible objects + events.
- **Multi-token goals**: require a set of tokens in one run (e.g. egg taken and
  opened).

This overlaps with multi-goal credit (idea 1). Start there; only change the goal
representation if bundles are insufficient.

### 9. Parallel search improvements

The accounting bug is fixed, but workers still duplicate effort.

- Assign disjoint goal shards or lease goals from a shared queue.
- Sync more often early (when knowledge is sparse) and less often later, or sync
  on new-token discoveries rather than a fixed game count.
- Consider a lightweight shared token/stable set so workers do not wait for a
  full checkpoint sync.
- Keep `--threads 1` reproducible.

### 10. Checkpoint autosave and hygiene

Current behavior: no timer. Saves happen every 10,000 games in single-threaded
mode, every 10,000 *completed-this-invocation* games in parallel mode, and on
exit.

Problems:

- Parallel uses `completed`, not `master.numplays`, so a resumed run that does
  not cross a 10k boundary from zero only saves on exit.
- Ctrl-C / kill in `--no-ui` loses everything since the last boundary.
- The checkpoint is ~1.1 MB at 33k games and grows with `tokens × best path`.
- `cmds` vectors grow forever; `best` paths duplicate long command prefixes.

Ideas:

- Add `--save-every N` (games) and `--save-interval S` (seconds); use
  `master.numplays` for the boundary in both modes.
- Save on SIGINT/SIGTERM; keep the existing temp-file + rename atomic write.
- Serialize best paths as a shared command-prefix trie or intern command strings.
- Prune old `cmds` entries and best paths for retired/stable tokens if they are
  no longer needed for hackvm.
- Consider a cheap append-only journal for crash recovery, then compact.

### 11. TUI / observability

Not throughput work, but it makes debugging the search much easier.

- Implement the `map` pane from the retained object tree and room graph.
- Show failure count / `score_tries` / goal type in the tokens pane.
- Add filters for token classes (`text`, `@mv`, `@f`, `@pp`).
- Show the command-model's top predictions for the current room.
- Add a raw/label toggle for troubleshooting.

## Project hygiene / parity leftovers

From `NEXT-STEPS.md`, still open:

- Investigate the two replay differences at turns 193/197 in Mini-Zork.
- Explain the score-rate gap versus `search.js` (HACK frequency differed in the
  old comparison).
- Make the replay parity harness a script and run it on every story.
- Make `curses.z3` useful as a target (it quits early because `die` is a verb).
- Decide where `search.js` lives; decide whether to commit the
  `encrusted-heart` changes.
- `folly` integration (blocked by armrest/tensorflow).

Open design question: is byte-level parity with `search.js` still a goal? The
recent heuristic changes deliberately diverge. If parity matters, keep them
behind flags; otherwise update the parity table and comparisons.

## Suggested order

1. Evaluation harness (`--stats` / `tools/bench.sh`).
2. Separate actionable goals from ambient NPC/world changes.
3. Multi-goal credit and token bundles.
4. Command template mining and mutation.
5. Context collection (room/inventory/objects) + backoff command model.
6. Inference chains / precondition priors.
7. Save-state tree search.
8. Early exit, heuristic tuning, parallel sharding.
9. Checkpoint autosave, pruning, and TUI polish in parallel with the above.

The first four are small enough to land and measure quickly. Save-state search
is the largest payoff but should come after the harness and multi-goal credit so
we can tell whether it is actually winning.

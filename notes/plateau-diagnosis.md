# Why the search is stuck on the scored Mini-Zork cluster

Status: implemented. `stablecount` is additive across workers, the coordinator
promotes from the summed value, score decay uses `goalruns` (option 1), the
bonus is rescaled to `scoremult = 15` and capped at 150, and old checkpoints are
repaired on load. `--stats` and the `goals` report print the terms that used to
be hidden. See the regression tests in `tests/search.rs`.

## TL;DR

The checkpoint currently has **72,254 plays, 892 tokens, 148 stable** goals.
Essentially every stable token was first seen on turn 0-2; only four were first
seen after that (first = 36-39). New tokens and new stable tokens have both
stopped.

Two production-only merge bugs cause this, and they compound:

1. **`stablecount` is merged with `max`, not summed** across workers, so it grows
   at roughly `total_workers_successes / thread_count`. Deep goals therefore
   never reach their stability threshold. This is the "no goals get stable"
   symptom.
2. **`score_tries` is merged with `min`** on equal score, and the minimum is
   usually 0 (a worker that never targeted the token). The score-decay feature
   therefore never advances, and the `-maxscore * 100` bonus is permanently at
   full strength. The goal queue's top 10 is therefore always the same
   already-scored cluster (dark cellar / studio-painting / troll room / nailed
   shut door), which is the "fixated on shut dark red walls" symptom.

A third, independent issue remains after those two are fixed: the score bonus
itself is two orders of magnitude larger than every other priority term, so even
with correct decay it pins the queue for far too long unless it is also rescaled
or capped.

## What the checkpoint says

```
$ python3 ... minizork.z3.save
plays 72254  tokens 892  stable 148
sum goalruns 72246  sum goalsucc 142439
tokens with goalruns>0        713
tokens never targeted         179
tokens with goalsucc==0       306   (302 of them have a best path)
stable tokens by `first`: {0:19, 1:102, 2:23, 36:1, 38:2, 39:1}
```

The current goal queue (recomputed from the saved stats):

```
-4146.6  You are in a dark, damp cellar      first 37  runs 674  succ 659  score 42
-4145.0  painting                            first 39  runs 688  succ 674  score 42
-4144.7  @mv_61_30 (painting -> you)         first 39  runs 718  succ 703  score 42
-4142.7  up, and looks climbable             first 40  runs 554  succ 538  score 42
-4139.0  fit with what you're carrying.      first 47  runs 680  succ 672  score 42
-4125.5  A painting by a neglected geni...   first 59  runs 440  succ 428  score 42
-4114.5  Some paint chips away, reveali...   first 71  runs 400  succ 390  score 42
-4094.4  @mv_30_158                           first 38  runs 229  succ 193  score 42
-3994.6  Cellar                              first 37  runs 474  succ 388  score 42
-3679.7  This is a crooked corridor fro...   first 95  runs  42  succ   0  score 42
```

All top-10 goals are the same Mini-Zork cellar/studio/painting route, and all
have `maxscore = 42`. 60 tokens in total have `maxscore = 42`, and 118 have
`maxscore = 10`.

A 400-game single-threaded resume from a copy of the checkpoint confirms the
plateau is structural, not just one unlucky seed:

```
plays=72654 stable=149   (+1 stable in 400 games)
tokens=892              (+0 new tokens)
top10_share=0.7375
explore_share=0.2000
frontier_share=0.0625
```

## Root cause 1: `stablecount` is max-merged

`src/checkpoint.rs`, both `merge` and `merge_delta`:

```rust
stat.stablecount = stat.stablecount.max(p.stablecount);
```

`goalruns` and `goalsucc` are summed in `merge_delta`:

```rust
stat.goalruns = stat.goalruns.saturating_add(p.goalruns);
stat.goalsucc = stat.goalsucc.saturating_add(p.goalsucc);
```

Workers keep their own state between batches and only reload a snapshot on a
sync (`src/parallel.rs`). Worker A sees its own `+k` successes, worker B sees its
own `+k`; the master keeps `max(k)`, not `sum(k)`. With T workers the effective
`stablecount` grows at roughly `goalsucc / T`.

The checkpoint shows exactly that:

| token | goalsucc | stablecount | threshold `5*(first+1)` |
| --- | ---: | ---: | ---: |
| You are in a dark, damp cellar | 659 | 187 | 190 |
| painting | 674 | 176 | 200 |
| @mv_61_30 | 703 | 184 | 200 |
| up, and looks climbable | 538 | 196 | 205 |
| A painting by a neglected geni... | 428 | 143 | 300 |

`stablecount` should be roughly `goalsucc` minus a few failure decrements. The
ratio `goalsucc / stablecount` is ~3.5-4, which matches a 4-thread run.
`src/parallel.rs`'s own comment says "counters and `stablecount` accumulate
instead of being reset every ten games" -- the merge is what defeats it.

**Effect:** a goal with `first = 37` needs 190 successes to become stable. The
worker that reaches it 184 times is discarded because another worker only reached
it 180 times. After several syncs it sits just below the threshold forever.

## Root cause 2: `score_tries` is min-merged

`src/checkpoint.rs`, both merge paths:

```rust
if p.maxscore > stat.maxscore {
    stat.maxscore = p.maxscore;
    stat.score_tries = incoming_tries;
} else if p.maxscore == stat.maxscore && incoming_tries < stat.score_tries {
    stat.score_tries = incoming_tries;
}
```

The intent of `score_tries` is "attempts since this token last improved its
score", feeding the decay in `src/search.rs`:

```rust
let score_bonus = stat.maxscore as f64 * self.scoremult
    / (1.0 + stat.score_tries as f64 / self.score_decay.max(1.0));
```

But taking the **minimum** on equal score means any worker that has not targeted
the token recently (score_tries = 0) resets the clock. In practice the clock is
always near 0: almost every token in the checkpoint has `score_tries` 0-3 even
with 400-700 `goalruns`. The bonus never decays.

**Effect:** `maxscore * 100` is ~4200 for the 60 score-42 tokens, while the rest
of the priority (distance + run/success logs +/- failure penalty) is in the tens
to hundreds. The 10 cheapest goals are permanently the score-42 cluster. 74% of
games replay them. Exploration (20%) and frontier (8%) are the only games that
target anything else.

## Root cause 3 (still present after 1 and 2): the score bonus is too big

Even with a correct, monotonically advancing decay, `maxscore = 42` gives
4200 points until ~200 attempts have accumulated, and a single lucky
`maxscore = 25` on a new deep token gives ~2000 points. The queue then jumps to
the *rarest* high-score tokens instead of the reachable frontier.

Simulation on the saved stats (goalruns used as the decay input):

| formula | top-10 composition |
| --- | --- |
| current (`scoremult=100`, tries=0) | 10/10 are score-42 tokens, mean first 50 |
| `goalruns` decay, `scoremult=100` | 0/10 are score-42; mostly one-shot depth-238 score-25 tokens |
| `goalruns` decay, `scoremult=20`, bonus capped at 200 | 0/10 score-42; attic/knife/rope cluster, mean first 38 |
| `goalruns` decay, `scoremult=10`, bonus capped at 100 | 1/10 score-42; attic/knife/rope cluster, mean first 37 |

The `scoremult=10-20`, cap 100-200 variants put the queue on reachable,
not-yet-stable, mid-game tokens (attic, knife, rope, flags 36-38) -- exactly the
layer that should become stable next.

## Potential fixes

### Fix 1 (required): make `stablecount` additive across workers

Treat `stablecount` like `goalruns`/`goalsucc`: the worker sends the *change
since its baseline* and the master adds it.

- Extend `Search::counter_snapshot` to also return `stablecount` (it can be a
  signed delta, because `endgame` subtracts 2 on a miss):

  ```rust
  pub fn counter_snapshot(&self) -> HashMap<String, (u64, u64, u64, i64)> { ... }
  ```

- In `parallel::worker`, subtract the baseline for `stablecount` too, along with
  `count`/`goalruns`/`goalsucc`.
- In `merge_delta`, use `stat.stablecount = stat.stablecount.saturating_add(p.stablecount)`
  (signed `i64`, so it can go down).
- Leave the idempotent `merge` as max if it is only used by tests, or give it a
  clear "re-merge same snapshot" contract. Production only uses `merge_delta`.

After this, a token's effective `stablecount` is the sum of every worker's
successes since the last sync, which is what `stablethresh` expects.

**Migration for existing checkpoints:** the saved `stablecount` already
under-counts by ~T. Options: (a) accept it and let it recover, (b) on load, if
`score_tries == 0 && maxscore > 0`, seed it from `goalruns`, and (c) optionally
recompute `stablecount` from `goalsucc` and the failure decrements, or simply
un-stable everything and let it re-stabilize. Recomputing from
`3*goalsucc - 2*goalruns` is approximately right except for replay-bundle credits
and the floor at 0.

README does not yet describe a way to repair a checkpoint; add a one-line
`--recompute-stability` (or do it automatically for old blobs) if the current
file matters.

### Fix 2 (required): fix or replace the score-decay counter

Three options, in increasing order of change:

1. **Simplest/merge-safe: decay on `goalruns`.** Remove `score_tries` from the
   formula:

   ```rust
   let score_bonus = stat.maxscore as f64 * self.scoremult
       / (1.0 + stat.goalruns as f64 / self.score_decay.max(1.0));
   ```

   `goalruns` is already additive and monotone; no merge changes and no migration
   math. Cost: improving a score does not reset the clock. That is acceptable:
   the numerator still rises with `maxscore`.

2. **Keep reset-on-improvement with signed deltas.** Store `score_tries` in the
   worker baseline, send `score_tries_now - baseline` as a signed delta, and add
   it. A reset produces a negative delta; clamp at 0 in the master. This is more
   code and needs a signed field (or a separate delta field) in `PersistStat`.
   It also needs a regression test for the reset case.

3. **Replace with `score_runs`.** Store the `goalruns` at which `maxscore` last
   improved and compute `tries = goalruns - score_runs`. This survives merges
   better than a reset counter but still needs careful frame handling when
   `goalruns` is summed across workers. Probably not worth it over option 1.

### Fix 3 (recommended): rebalance the score bonus

- Lower `scoremult` from 100 to ~10-20, and/or cap the bonus:

  ```rust
  let score_bonus = (stat.maxscore as f64 * self.scoremult
      / (1.0 + tries / self.score_decay.max(1.0)))
      .min(SCORE_BONUS_CAP); // e.g. 100-200
  ```

- Keep `w_stable = 1000` well above the cap so stable goals stay out of the
  queue.
- Add a test that asserts a score-42 token with many attempts sorts *below* a
  reachable non-stable, non-max-score token, and that a brand-new score-25 token
  does not immediately pin the queue.

### Fix 4 (optional): add an explicit anti-fixation quota

The priority queue can legitimately want to retry a scored route; the problem is
that 74% of games went to the same 10 entries. Independent of the score formula:

- When drawing the normal top-10 candidates, skip tokens already at the best
  known score for their `first` band, or reserve one slot for "not stable and not
  already scored".
- Extend `frontier_goal` to include deep tokens that *have* succeeded but are not
  stable (currently it requires `goalsucc == 0`), which is where the attic/knife/
  rope layer lives.
- Consider a recency term (games since last targeted) so the top 10 rotates.

### Fix 5 (observability): make this visible next time

- Add a `--goals` / queue-detail output that prints each priority term
  (`distance`, `untried`, `success`, `failure`, `stable`, `score`, `tries`), not
  just the total. The total was negative by 4000 points and hid the cause.
- Add a `--stats` field: `score_tries_p50/max` and `mean goalsucc/stablecount`
  ratio. A value near the thread count is the fingerprint of root cause 1.
- Persist a small `run_meta` block (thread count, last sync, code version) in the
  checkpoint so a stuck blob can be explained after the fact.

## Regression tests to add

1. **Stable accumulation:** two workers each credit the same goal `k` times;
   after `merge_delta` its `stablecount` (and, at threshold, stable set) equals
   the sum, not the max.
2. **Score decay advances:** merging a batch that increments `score_tries` must
   raise the merged value; a stale zero from another worker must not reset it.
3. **Queue rotates:** build stats with N score-42 tokens with 500 runs each and a
   reachable non-stable token; assert the non-stable token is in the top 10 after
   the fix.
4. **Checkpoint migration:** load a blob with `stablecount << goalsucc`; assert
   the repaired/recomputed state does not leave goals permanently one success
   short of `stablethresh`.

## Suggested order

1. Fix 1 (stablecount additive) -- this unlocks every goal that is already one
   or two successes short of stable.
2. Fix 2 option 1 (goalruns decay) -- one-line formula change, merge-safe,
   immediately unsticks the queue.
3. Fix 3 (rescale/cap the score bonus) -- tune with a fixed-seed 1-thread A/B,
   then confirm with 4 threads.
4. Fix 1 migration for the existing `minizork.z3.save` (or start a fresh
   checkpoint and keep the old one for comparison).
5. Re-run the 400-game single-thread baseline and compare:
   new tokens, new stable tokens per 1k games, top10 share, useful-command rate,
   and mean `first` of the top 10.

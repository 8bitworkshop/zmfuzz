# Roadmap: from token search to state search

`ideas.md` is the idea inventory. `next-steps.md` is the old handoff/parity note.
This file is the *order*: what depends on what, and what measurement decides
whether a stage was worth it.

Rule for every stage below: **it must move a `tools/bench.sh` metric or it gets
reverted.** One thread, fixed seed, same story/turns, `diff before.txt after.txt`.
The metrics that matter, in priority order:

| metric | means |
| --- | --- |
| `newtokens_per_1k_turns` | the actual goal: novel output per unit of VM time |
| `tokens` | breadth reached in a fixed budget |
| `useful_command_rate` | turns that were not parser noise or no-ops |
| `state_change_rate` | turns that moved an object / set a flag |
| `stable_per_1k_games` | goals actually reproducible, not just seen once |
| `avg_path_len` | whether we are learning routes or just touching tokens |

If a stage raises `useful_command_rate` but lowers `newtokens_per_1k_turns`, it
has collapsed onto familiar territory. That is a revert, not a tuning problem.

## Stage 0 — Near-miss turns and precondition facts

Design: [`near-miss.md`](near-miss.md). Status: **0a landed** (refused turns
are classified and mined as templates; +8% useful/state-changing turns at 400
games, new tokens flat). Next: the startup probe, then 0b slot completion.

The search currently discards every command that was understood but refused.
Those refusals are simultaneously (a) free grammar-accepted command templates,
(b) hints naming a missing slot, and (c) the raw material for learned
`requires(...)` facts. This is first because it is cheap, it attacks the
measured bottleneck (most turns are parser errors), and it produces the labelled
data every later stage needs.

Gate: `useful_command_rate` and `state_change_rate` up, `newtokens_per_1k_turns`
not down.

## Stage 1 — State-conditioned backoff command model

Supersedes the uniform `sample_word` in `template_cmd` (`src/search.rs:1690`).
Counts of `(verb, noun)` that succeeded or were merely understood, keyed by
`(room, visible objects, inventory, last outcome class)`, with backoff to global
counts plus additive smoothing and a retained exploration share.

Note the "Markov" framing that does *not* apply: commands are 1–2 words, so an
n-gram over command words has nothing to learn. The Markov structure here is
*state → command*, not word → word.

Depends on: stage 0 (the outcome labels and the refusal/success split).
Gate: `useful_command_rate` up with `newtokens_per_1k_turns` flat or up.
Kill criterion: the model predicts one command per room and `tokens` stalls —
raise the exploration share before concluding the model failed.

## Stage 2 — Snapshot branching (the actual "tree")

Best-first search over VM snapshots: snapshot at each prompt, expand the most
promising frontier node with a `template_cmd` mutation, score children by
*novelty* (new tokens / new room / new state fingerprint). This is MCTS minus
the Monte Carlo. Replaces sequential replay, which is where `DIVERGE` comes from:
branching from a known state cannot diverge.

Depends on: stage 1 (branching multiplies whatever command quality you have —
branching over parser errors is just more parser errors) and the VM snapshot
API in `ideas.md` §5. Go/no-go from §5 applies: snapshot-restore must reproduce
replay's token stream exactly before any statistics touch it.

Gate: tokens per CPU-second up at equal wall time; `avg_path_len` shortens;
`diverged_turns` becomes structurally impossible rather than merely rare.

## Stage 3 — Coverage archive instead of per-token goals

MAP-Elites over state fingerprints `(room, inventory, flags)`, keeping one elite
path per cell. This is the principled fix for the class of plateau documented in
`plateau-diagnosis.md`: per-token goals can pin a cell, a coverage archive cannot,
because a full cell stops attracting games. It also makes `first`-based priority
much less load-bearing.

Depends on: stage 2 (cheap revisits are what make a cell's elite worth keeping,
and fingerprints need the snapshot/state read path).

Gate: distinct cells per 1,000 games, and `stable_per_1k_games`.
Kill criterion: cells saturate while `newtokens_per_1k_turns` keeps falling —
that is aliasing (too many states in one cell, or the fingerprint ignores what
matters), not coverage.

## Not on the roadmap: vanilla MCTS / UCT

Recorded so it does not get re-litigated:

- branching factor is `verbs × nouns` at every node, thousands-wide;
- rollouts are a VM step loop to the next prompt and mostly return parser
  errors, so a uniform-policy rollout gives close to zero signal;
- the reward (novelty) is sparse and non-stationary, which is exactly what UCT
  handles worst.

MCTS only becomes interesting after stage 1 gives a policy prior and stage 2
gives snapshots — i.e. PUCT. That is a lot of scaffolding for a fuzzer, and
stages 2 + 3 get most of the coverage benefit without it.

Also deliberately deferred: suffix splicing / crossover of `best` paths
(`ideas.md` §5) — a run is stateful, so a borrowed suffix is only meaningful
from an equivalent state, and stage 2 is what makes that checkable. And a
STRIPS-style planner: it needs `requires` facts that stage 0 has not collected
yet, so it is premature by construction.

## Why this order

```
stage 0  ──>  stage 1  ──>  stage 2  ──>  stage 3
labels        policy        state         coverage
```

Each arrow is "the later stage's evidence source is the earlier stage's output":
refusals label the model, the model gives expansions worth branching on,
branching makes revisits cheap enough to archive. Doing 2 before 1 pays full VM
cost to explore a tree whose edges are mostly parser errors. Doing 3 before 2
produces an archive nobody visits.

Cost/benefit if only one more stage happens: 0 is the cheapest and most likely
to move a metric; 1 is the largest single jump in useful turns per line of code;
2 is the largest structural change and the one that needs the VM work; 3 is the
smallest of the three and mostly rides on 2.

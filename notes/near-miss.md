# Stage 0: near-miss turns and precondition facts

Roadmap position: [`roadmap.md`](roadmap.md) stage 0. The cheapest change that
attacks the measured bottleneck (most turns are parser errors) and the data
layer the later stages consume.

## The signal being thrown away

`committurn` (`src/search.rs:1474`) counts a turn as useful only when it changed
a room, changed state, or revealed new text (`:1528`). A command the game
*understood* and then refused fails all three, so it is recorded in `playthru`
and then ignored. That is the single most informative turn class in the game.

For a troll encounter there are four distinct outcomes, and they must not be
lumped together:

| class | example shape | what it tells us |
| --- | --- | --- |
| `UnknownWord` | "I don't know the word kark." | grammar only, discard |
| `UnseenNoun` | "You can't see any sword here." | a noun to find, plus where it isn't |
| `Refused` | "You can't kill the troll with your bare hands." | **understood command, unmet precondition, open slot named** |
| `Useful` | troll moves / dies, new text | existing behaviour |

`Refused` is the class that answers "sword → you is needed to kill the troll".
`UnseenNoun` is the class that answers "and it isn't here".

## Status: 0a landed

Classified and mined; no probe yet. `committurn` now sends every turn it used
to drop into one of two pools: turned a profit (`useful`, unchanged) or was
understood but achieved nothing (`refused`). Refused commands go into a capped
`REFUSAL_POOL` and are drawn by `template_cmd` on `REFUSAL_WEIGHT = 0.35` of the
template draws; `--stats` reports `refusal_rate`.

`tools/bench.sh minizork.z3 400 200 1`, before → after (raw output in
`bench/0a-400-*.txt`):

| metric | before | after |
| --- | ---: | ---: |
| `tokens` | 344 | 347 |
| `stable` | 22 | 22 |
| `newtokens_per_1k_turns` | 4.300 | 4.338 |
| `useful_command_rate` | 0.2306 | 0.2492 |
| `state_changes_per_turn` | 0.7691 | 0.8238 |
| `avg_path_len` | 54.28 | 46.53 |
| `replay_divergence_rate` | 0.0041 | 0.0120 |

Gate result: useful turns and state changes up ~8% relative, new tokens flat,
so 0a stays. Two caveats, both expected from this doc:

- **`refusal_rate` is not yet meaningful.** It reports 0.75, because almost any
  command built from dictionary words is "understood" by this test — the cheap
  classifier is close to "not useful". The probe below is what makes it a real
  signal, and it is the prerequisite for 0b/0c needing to know *why* a command
  failed.
- **Divergence tripled** (0.4% → 1.2%) even though it is tiny in absolute
  terms. The likely cause is replaying commands that never reproduced anything
  in the first place. If 0b does not erase it, cap refused commands out of the
  replay set specifically.

One latent bug surfaced and fixed on the way: `set_stats` replaced the token
table but left `Score` entries for tokens that no longer existed, so stale
entries could outrank live goals. It now clears the queue, and callers reindex.

## Classifying outcomes story-agnostically

Two layers, both cheap:

1. **Host layer (exact).** The Z-machine parse buffer stores a zero dictionary
   address for every word the parser did not recognise (`tokenise`,
   `encrusted-heart/src/zmachine.rs:607`), and `get_dictionary()` already gives
   the word list. So `UnknownWord` is decidable without asking the game: if any
   word in the command is absent from the dictionary, it is `UnknownWord`.
   Optionally expose the per-word parse-buffer entries for a sturdier check.
2. **Game layer (probed once).** The game's own verb table is a second parser we
   cannot see. Learn its complaints instead: at startup build a throwaway
   `Zmachine` (same pattern as `machine()`, `src/search.rs:794`) and send a
   handful of probes — `xyzzy`, `take xyzzy`, `xyzzy the grue`, `take grue`.
   Those responses are the game's unknown-word and cannot-see-any templates.
   Store them (in memory) and match future responses against them.

Classification after the probe:

```
any word not in dictionary            -> UnknownWord
matches a probed parser-error template -> UnknownWord / UnseenNoun
changed room, state, or revealed text  -> Useful
otherwise                              -> Refused
```

Fallback if the probe finds nothing stable (some games are uniformly polite): a
response that changed nothing, whose words are all known, and that echoes the
verb and noun is a *weak* refusal. Keep weak and strong refusals in separate
counters so the metric cannot be fooled.

## Data model

In memory first. The checkpoint is already ~5 MB; none of this is persisted
until a stage 0 metric proves it belongs there.

```rust
enum Outcome { UnknownWord, UnseenNoun, Refused, Useful }

struct NearMiss {
    words: Vec<String>,      // the command as sent
    room: String,            // self.room at the time
    outcome: Outcome,
    missing: Vec<TokenId>,   // dict nouns named in the response, not visible/present
}

// keyed by (room, verb, noun); verb/noun are the command's words
success:  HashMap<(String, String, String), u32>,
refusals: HashMap<(String, String, String), NearMiss>,   // capped, reservoir like cmdtemplates
requires: HashMap<(String, String), Vec<(String, TokenId, u8)>>,  // verb, noun -> slot evidence
```

`refusals` is capped exactly like `cmdtemplates` (`TEMPLATE_POOL`,
`src/search.rs:1596`) so a story cannot grow it without bound.

## Sub-steps, each independently shippable

**0a — refusals are templates.** **Landed** (see the status table above).
Route `Refused` turns through
`record_template` (`:1589`) into a tagged pool. Today a refusal is dropped; after
0a `kill troll` is a known grammar-accepted shape within a few turns of first
trying it. Expected effect: `useful_command_rate` up by making subsequent turns
mutate a command the parser accepted instead of drawing two random words.

**0b — slot completion from inventory.** `template_cmd` (`:1680`) currently
replaces one slot with `sample_word` — uniform over the dictionary. Extend it:
for a template with an open prepositional slot, prefer a word naming a child of
the player object. Contents come from `get_object_tree()`
(`encrusted-heart/src/zmachine.rs:855`) via `Labels` (`src/labels.rs:33`,
`player()` at `:86`). Inventory is usually 5–10 items, so `kill troll with X`
costs tens of turns, not thousands. Gate re-attempts on an inventory fingerprint
change so the same failing command is not resent every turn.

**0c — refusal-named nouns become subgoals.** A `UnseenNoun`/`Refused` response
whose text names a dictionary noun that is not in inventory and not visible
becomes a subgoal. If `@mv_<obj>_<player>` exists, that token is already a
non-ambient goal with a `best` path — queue it. This is backwards chaining over
machinery that already exists, and it is the "figure out how to get it" half.

**0d — differential precondition learning.** Same command, one slot differs,
outcome flips `Refused` → `Useful`: emit `requires(verb, noun) => slot_value`
with n-of-m evidence, same room, no intervening inventory change. Deferred past
0a–0c because it needs their data, and it is the part that can learn a wrong
fact from coincidence.

## Touch points

| file | change |
| --- | --- |
| `src/search.rs:1474` `committurn` | classify the turn, record `NearMiss`, new counters |
| `src/search.rs:1589` `record_template` | tag templates with the outcome that produced them |
| `src/search.rs:1680` `template_cmd` / `:1657` `sample_word` | inventory-biased slot fill |
| `src/search.rs:1242` `logprint` | harvest refusal-text nouns (already collects `playvocab`) |
| `src/search.rs:812` `learn_labels` | keep the player object and its children per turn |
| `src/search.rs:794` `machine` | throwaway probe VM at startup (separate seed; no effect on game RNG) |
| `src/main.rs` `--stats` | `refusal_rate`, `slot_completions`, `precondition_facts` |

The probe runs on its own `Zmachine` and produces no tokens, so determinism
tests (same seed ⇒ same trace) stay valid. Guard it with `IGNORED_WORDS`
(`src/search.rs:28`) so it can never send `save`/`restore`/`quit`.

## Evaluation

Add the three counters above, then the gate is the roadmap gate: baseline
`tools/bench.sh` first (keep `before.txt`), and after 0a+0b require
`useful_command_rate` and `state_change_rate` up with
`newtokens_per_1k_turns` flat or better. If `newtokens_per_1k_turns` drops while
`useful_command_rate` rises, the inventory bias is over-fitting one room —
shrink the bias, do not delete the stage.

## Risks

- **Mini-Zork's troll is not a single key.** The axe and the thrown knife also
  work. Everything here is a *prior for ordering*, never a proof; keep the
  random exploration share so an alternative solution is still reachable.
- **Generic refusals.** "You can't do that." names nothing. That is why 0b is
  enumeration and 0c is opportunistic, not the other way round.
- **Spurious preconditions** from room changes and NPC randomness. Same-room,
  no-intervening-state-change, n-of-m evidence is the minimum bar.
- **New-text accounting.** A refusal sentence is genuinely new text, so the
  first occurrence is useful and will be mined; that is correct, but do not let
  probe output enter the token interner or the token count will be inflated by
  four synthetic strings.
- **Weak-refusal false positives** inflate `useful_command_rate` without
  producing progress. That is exactly why the gate also watches
  `newtokens_per_1k_turns` and why weak and strong refusals get separate
  counters.

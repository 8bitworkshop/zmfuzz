//! The search itself: a port of `GameRunner` from `search.js`.
//!
//! One `play_game()` is one run: a fresh machine, a goal token, up to
//! `maxturns` commands, then statistics folded back into the shared tables.
//! The heuristics are deliberately the JS ones, constants included, so traces
//! from the two implementations can be compared.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use encrusted_heart::options::Options;
use encrusted_heart::traits::VmEvent;
use encrusted_heart::zmachine::{Step, Zmachine};
use encrusted_heart::zscii::ZChar;

use crate::labels::Labels;
use crate::rng::Rng;
use crate::stat::{Playthrough, Score, Stat, FIRST_UNKNOWN};
use crate::token::{Interner, TokenId};
use crate::ui::TokenUI;

/// How long the VM may run without asking for input before we give up on the
/// run. `search.js` would simply hang here; a fuzzer cannot afford that.
const MAX_INSTRUCTIONS_PER_INPUT: usize = 200_000;

/// Words filtered out of the dictionary: they end or restart the game.
const IGNORED_WORDS: [&str; 6] = ["restor", "restar", "save", "q", "quit", "zzkjlk"];

/// How many under-explored tokens form the pool an exploration game draws
/// from. Kept small so exploring still follows the cheapest-first spirit.
const EXPLORE_POOL: usize = 32;

/// How many deep, unsolved known paths form the frontier pool. The normal
/// priority queue is deliberately cheapest-first, which makes a goal first seen
/// at turn 300 effectively untargetable; a small frontier quota ensures the
/// search keeps trying to reproduce the deepest thing it has ever seen.
const FRONTIER_POOL: usize = 32;

/// A verified replay can retire several goals at once, but crediting every
/// token on a long path would make easy parser responses stable overnight.
const MAX_REPLAY_CREDITS: usize = 8;

/// Successful commands kept as templates. Small enough to stay cheap and to
/// keep the model from collapsing onto one shape.
const TEMPLATE_POOL: usize = 256;

/// A best path at least this long is replayed as a sequence. Retrying only the
/// command that produced the token works when the goal is a turn or two away;
/// for a far-away goal it throws away the whole route that got there.
const LONG_PATH_TURNS: isize = 12;

/// Extra turns a game gets when its goal was first seen beyond the normal turn
/// limit. Without this a deep goal can be selected forever and never reached.
const TURN_STRETCH_FRACTION: isize = 4;

/// Monotonic run counters that can be summed across parallel workers. The
/// state-derived parts of [`Metrics`] come from the merged token table instead.
#[derive(Debug, Clone, Copy, Default)]
pub struct RunCounters {
    pub turns: u64,
    pub useful_turns: u64,
    pub state_turns: u64,
    pub newtokens: u64,
    pub state_changes: u64,
    pub replay_turns: u64,
    pub diverged_turns: u64,
    pub goal_games: u64,
    pub top10_games: u64,
    pub explore_games: u64,
    pub frontier_games: u64,
}

impl RunCounters {
    /// Counters added since `earlier`, saturating rather than wrapping.
    pub fn delta_since(&self, earlier: &RunCounters) -> RunCounters {
        RunCounters {
            turns: self.turns.saturating_sub(earlier.turns),
            useful_turns: self.useful_turns.saturating_sub(earlier.useful_turns),
            state_turns: self.state_turns.saturating_sub(earlier.state_turns),
            newtokens: self.newtokens.saturating_sub(earlier.newtokens),
            state_changes: self.state_changes.saturating_sub(earlier.state_changes),
            replay_turns: self.replay_turns.saturating_sub(earlier.replay_turns),
            diverged_turns: self.diverged_turns.saturating_sub(earlier.diverged_turns),
            goal_games: self.goal_games.saturating_sub(earlier.goal_games),
            top10_games: self.top10_games.saturating_sub(earlier.top10_games),
            explore_games: self.explore_games.saturating_sub(earlier.explore_games),
            frontier_games: self.frontier_games.saturating_sub(earlier.frontier_games),
        }
    }

    pub fn add(&mut self, other: &RunCounters) {
        self.turns = self.turns.saturating_add(other.turns);
        self.useful_turns = self.useful_turns.saturating_add(other.useful_turns);
        self.state_turns = self.state_turns.saturating_add(other.state_turns);
        self.newtokens = self.newtokens.saturating_add(other.newtokens);
        self.state_changes = self.state_changes.saturating_add(other.state_changes);
        self.replay_turns = self.replay_turns.saturating_add(other.replay_turns);
        self.diverged_turns = self.diverged_turns.saturating_add(other.diverged_turns);
        self.goal_games = self.goal_games.saturating_add(other.goal_games);
        self.top10_games = self.top10_games.saturating_add(other.top10_games);
        self.explore_games = self.explore_games.saturating_add(other.explore_games);
        self.frontier_games = self.frontier_games.saturating_add(other.frontier_games);
    }
}

/// Version-independent snapshot of the search's work, for `--stats` and
/// `tools/bench.sh`. Ratios are left to the caller so the raw counters stay
/// meaningful when comparing runs.
#[derive(Debug, Clone, Default)]
pub struct Metrics {
    pub plays: u64,
    pub turns: u64,
    pub tokens: usize,
    pub stable: usize,
    pub newtokens: u64,
    pub useful_turns: u64,
    pub state_turns: u64,
    pub state_changes: u64,
    pub replay_turns: u64,
    pub diverged_turns: u64,
    pub goal_games: u64,
    pub top10_games: u64,
    pub explore_games: u64,
    pub frontier_games: u64,
    pub negative_priorities: usize,
    pub never_targeted: usize,
    pub avg_path_len: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameEnd {
    /// The game quit (or crashed); `search.js` calls this `DeadError`.
    Quit,
    /// Out of turns after the final `SCORE`.
    Turns,
    /// The VM never asked for input.
    Stuck,
}

impl GameEnd {
    pub fn label(self) -> &'static str {
        match self {
            GameEnd::Quit => "ended",
            GameEnd::Turns => "turn-limit",
            GameEnd::Stuck => "stuck",
        }
    }
}

/// One turn's worth of state, handed to the live display once per turn.
#[derive(Debug, Clone, Default)]
pub struct Progress {
    pub turn: isize,
    pub goal: String,
    pub cmd: String,
    pub tokens: Vec<String>,
    /// Room name from the VM status bar, if the story draws one.
    pub room: String,
    /// The right half of the status bar: `score/moves`, or a clock.
    pub score: String,
}

/// Events kept for the display: enough to fill a screen, bounded so a
/// never-drained search cannot grow without limit.
const EVENT_BUFFER: usize = 512;

pub struct Search {
    story: Vec<u8>,

    // --- knobs, named after the search.js consts ---
    pub maxturns: isize,
    /// Turn limit for the game in progress. Normally `maxturns`; stretched when
    /// the chosen goal's known path is longer than the normal game.
    game_turns: isize,
    pub usewords: bool,
    pub usetech: bool,
    pub usevmhack: bool,
    /// jszm's `search.js` logs `store` but then ignores it (`return` before the
    /// `switch`), so VM-stores are off by default to keep traces comparable.
    pub record_stores: bool,
    pub wordtoklen: usize,
    pub prob_vocab: f64,
    pub prob_end: f64,
    pub stablethresh: i64,
    pub prescoretok: Option<String>,
    pub scoremult: f64,
    pub w_distance: f64,
    pub w_untried: f64,
    pub w_success: f64,
    pub w_fail: f64,
    pub w_stable: f64,
    /// Goal attempts after which the score bonus has halved.
    pub score_decay: f64,
    /// Fraction of games sent to a least-tried token instead of the top ten.
    pub prob_explore: f64,
    /// Fraction of games sent to a deep, unsolved goal with a known path.
    pub prob_frontier: f64,
    /// Fraction of turns that mutate a successful command instead of drawing
    /// two random dictionary words.
    pub prob_template: f64,
    pub verbose: bool,
    pub log_instructions: bool,
    /// Suppress the per-run lines; `--stats` reports the aggregates instead.
    pub quiet_runs: bool,
    pub checkpoint_path: Option<PathBuf>,
    /// Called once per turn, if a live display is attached. It gets the search
    /// itself as well, so a single-threaded driver can redraw (and poll keys)
    /// without waiting for the game to end.
    pub progress: Option<Arc<dyn Fn(&Search, Progress) + Send + Sync>>,

    // --- knowledge, persists across games ---
    pub rng: Rng,
    pub tokens: Interner,
    pub numplays: u64,
    stats: HashMap<TokenId, Stat>,
    stabletoks: HashSet<TokenId>,
    /// Stable tokens in insertion order, for `recomputevmtoks`'s ordering.
    stableorder: Vec<TokenId>,
    vmtoks: Vec<TokenId>,
    tokfreq: BTreeSet<Score>,
    vocab: Option<Vec<TokenId>>,
    vocabdict: HashSet<TokenId>,

    // --- per-game state ---
    playtoks: HashSet<TokenId>,
    playvocab: Vec<TokenId>,
    playvocabset: HashSet<TokenId>,
    numturns: isize,
    goaltok: Option<TokenId>,
    goalmet: bool,
    turncmd: Option<String>,
    turntoks: Vec<TokenId>,
    turntokseen: HashSet<TokenId>,
    turnisreplay: bool,
    expectedtoks: Option<Vec<TokenId>>,
    replaydiverged: bool,
    nexttokisscore: bool,
    curscore: i64,
    hacks: Vec<TokenId>,
    playthru: Playthrough,
    /// Last room name the VM's status bar reported, and its score half.
    room: String,
    score: String,
    /// `NEWTOKEN` / `STABLE` / `HACK` / ... lines awaiting the display.
    eventbuf: VecDeque<String>,
    /// Object numbers to names, for the display only; see `labels`.
    labels: Labels,
    /// The object the player controls, once known. Used to keep NPC moves out
    /// of the goal queue: chasing the thief is not something a command can do.
    player_object: Option<u16>,
    /// Moves seen this turn, for learning the player object from a room change.
    turn_moves: Vec<(u16, u16)>,
    turn_room_changed: bool,

    // --- instrumentation, for `--stats` ---
    turns_played: u64,
    useful_turns: u64,
    state_turns: u64,
    newtoken_count: u64,
    state_changes: u64,
    replay_turns: u64,
    diverged_turns: u64,
    goal_games: u64,
    top10_games: u64,
    explore_games: u64,
    frontier_games: u64,
    replay_credits: usize,

    // --- command templates mined from commands that produced a token ---
    cmdtemplates: Vec<Vec<String>>,
    cmdtemplateset: HashSet<String>,
}

impl Search {
    pub fn new(story: Vec<u8>, seed: u64, maxturns: isize, verbose: bool) -> Search {
        Search {
            story,
            maxturns,
            game_turns: maxturns,
            usewords: true,
            usetech: true,
            usevmhack: true,
            record_stores: false,
            wordtoklen: 30,
            prob_vocab: 0.5,
            prob_end: 0.5,
            stablethresh: 5,
            prescoretok: Some("Your score is".to_string()),
            scoremult: 100.0,
            w_distance: 1.0,
            w_untried: 4.0,
            w_success: 2.0,
            w_fail: 10.0,
            w_stable: 1000.0,
            score_decay: 5.0,
            prob_explore: 0.2,
            prob_frontier: 0.08,
            prob_template: 0.35,
            verbose,
            log_instructions: false,
            quiet_runs: false,
            checkpoint_path: None,
            progress: None,

            rng: Rng::new(seed),
            tokens: Interner::new(),
            numplays: 0,
            stats: HashMap::new(),
            stabletoks: HashSet::new(),
            stableorder: Vec::new(),
            vmtoks: Vec::new(),
            tokfreq: BTreeSet::new(),
            vocab: None,
            vocabdict: HashSet::new(),

            playtoks: HashSet::new(),
            playvocab: Vec::new(),
            playvocabset: HashSet::new(),
            numturns: -1,
            goaltok: None,
            goalmet: false,
            turncmd: None,
            turntoks: Vec::new(),
            turntokseen: HashSet::new(),
            turnisreplay: false,
            expectedtoks: None,
            replaydiverged: false,
            nexttokisscore: false,
            curscore: 0,
            hacks: Vec::new(),
            playthru: Playthrough::new(),
            room: String::new(),
            score: String::new(),
            eventbuf: VecDeque::new(),
            labels: Labels::default(),
            player_object: None,
            turn_moves: Vec::new(),
            turn_room_changed: false,
            turns_played: 0,
            useful_turns: 0,
            state_turns: 0,
            newtoken_count: 0,
            state_changes: 0,
            replay_turns: 0,
            diverged_turns: 0,
            goal_games: 0,
            top10_games: 0,
            explore_games: 0,
            frontier_games: 0,
            replay_credits: 0,
            cmdtemplates: Vec::new(),
            cmdtemplateset: HashSet::new(),
        }
    }

    /// Turns the last game actually played.
    pub fn last_turns(&self) -> isize {
        self.numturns.max(0)
    }

    /// Restart the random stream. Used when resuming: the run continues from
    /// `base_seed + numplays`, so each invocation explores new ground instead
    /// of replaying the sequence the checkpoint already recorded.
    pub fn reseed(&mut self, seed: u64) {
        self.rng = Rng::new(seed);
    }

    pub fn version(&self) -> u8 {
        *self.story.first().unwrap_or(&0)
    }

    pub fn token_count(&self) -> usize {
        self.stats.len()
    }

    /// Best score any token has reached across the whole search.
    pub fn top_score(&self) -> i64 {
        self.stats
            .values()
            .map(|stat| stat.maxscore)
            .max()
            .unwrap_or(0)
    }

    pub fn vocab_size(&self) -> usize {
        self.vocab.as_ref().map(|v| v.len()).unwrap_or(0)
    }

    pub fn stable_count(&self) -> usize {
        self.stabletoks.len()
    }

    /// Top goal candidates, most attractive first.
    pub fn goal_queue(&self) -> Vec<(f64, TokenId)> {
        self.tokfreq
            .iter()
            .take(20)
            .map(|s| (s.value, s.token))
            .collect()
    }

    /// `(first, goalruns, goalsucc, stable, maxscore)` for goal-queue debugging.
    pub fn goal_detail(&self, id: TokenId) -> (usize, u64, u64, bool, i64) {
        match self.stats.get(&id) {
            Some(stat) => (
                stat.first,
                stat.goalruns,
                stat.goalsucc,
                self.stabletoks.contains(&id),
                stat.maxscore,
            ),
            None => (0, 0, 0, false, 0),
        }
    }

    pub fn current_room(&self) -> &str {
        &self.room
    }

    pub fn current_score(&self) -> &str {
        &self.score
    }

    /// The commands of the game in progress (or the last one played), in order.
    /// The synthetic SCORE turn is left out: it is not a command the search
    /// chose and would render as a dangling arrow.
    pub fn current_route(&self) -> Vec<String> {
        self.playthru
            .turns
            .iter()
            .filter(|turn| !turn.cmd.is_empty())
            .map(|turn| turn.cmd.clone())
            .collect()
    }

    /// Shortest known command path to `id`, or empty if none is recorded yet.
    pub fn best_path(&self, id: TokenId) -> Vec<String> {
        self.stats
            .get(&id)
            .and_then(|stat| stat.best.as_ref())
            .map(|turns| turns.iter().map(|turn| turn.cmd.clone()).collect())
            .unwrap_or_default()
    }

    pub fn token_name(&self, id: TokenId) -> &str {
        self.tokens.name(id)
    }

    /// Read the object table for display. The table is static, so once per
    /// search is enough.
    fn learn_labels(&mut self, zm: &Zmachine<TokenUI>) {
        if self.labels.is_empty() {
            self.labels = Labels::from_tree(&zm.get_object_tree().to_string());
            if self.player_object.is_none() {
                self.player_object = self.labels.player();
                if self.player_object.is_some() {
                    // Ambient goals may already be queued from a checkpoint;
                    // the queue is rebuilt without them.
                    self.reindex();
                }
            }
        }
    }

    /// Make sure labels exist without playing a game.
    ///
    /// The parallel coordinator never plays a game itself, so it would otherwise
    /// print raw ids in its summary. `machine()` would work but draws from the
    /// search RNG, and reading names must not disturb the stream.
    pub fn ensure_labels(&mut self) {
        if !self.labels.is_empty() {
            return;
        }
        let zm = Zmachine::new(self.story.clone(), TokenUI::new(), Options::default());
        self.learn_labels(&zm);
    }

    /// Readable form of a token, for display only. Never feed the result back
    /// into the search: identity is the raw string, and names are not unique.
    pub fn label(&self, token: &str) -> String {
        self.labels.label(token)
    }

    /// Readable form of an interned token.
    pub fn token_label(&self, id: TokenId) -> String {
        self.labels.label(self.tokens.name(id))
    }

    /// The object table, once a game has run and the story could be read.
    pub fn labels(&self) -> &Labels {
        &self.labels
    }

    /// The player object, once it is known.
    pub fn player_object(&self) -> Option<u16> {
        self.player_object
    }

    /// True when a token is a state change we cannot control: a move by any
    /// object other than the player (NPCs, mostly). It stays in the statistics
    /// and the display, but never becomes a goal.
    pub fn is_ambient(&self, id: TokenId) -> bool {
        let player = match self.player_object {
            Some(player) => player,
            None => return false,
        };
        match move_parts(self.tokens.name(id)) {
            // A move into the player's own inventory is player-driven even
            // when the object is not: taking the egg is a goal, the thief
            // wandering off is not.
            Some((object, parent)) => object != player && parent != player,
            None => false,
        }
    }

    /// Everything `--stats` needs. Cheap: one pass over the token table.
    pub fn metrics(&self) -> Metrics {
        let mut negative_priorities = 0;
        let mut never_targeted = 0;
        let mut path_total = 0usize;
        let mut path_count = 0usize;
        for id in self.stat_ids() {
            let stat = self.stat(id);
            if stat
                .priority
                .as_ref()
                .is_some_and(|score| score.value < 0.0)
            {
                negative_priorities += 1;
            }
            if stat.goalruns == 0 {
                never_targeted += 1;
            }
            if let Some(best) = &stat.best {
                path_total += best.len();
                path_count += 1;
            }
        }
        Metrics {
            plays: self.numplays,
            turns: self.turns_played,
            tokens: self.token_count(),
            stable: self.stable_count(),
            newtokens: self.newtoken_count,
            useful_turns: self.useful_turns,
            state_turns: self.state_turns,
            state_changes: self.state_changes,
            replay_turns: self.replay_turns,
            diverged_turns: self.diverged_turns,
            goal_games: self.goal_games,
            top10_games: self.top10_games,
            explore_games: self.explore_games,
            frontier_games: self.frontier_games,
            negative_priorities,
            never_targeted,
            avg_path_len: if path_count == 0 {
                0.0
            } else {
                path_total as f64 / path_count as f64
            },
        }
    }

    /// Monotonic counters for the parallel coordinator to aggregate.
    pub fn run_counters(&self) -> RunCounters {
        RunCounters {
            turns: self.turns_played,
            useful_turns: self.useful_turns,
            state_turns: self.state_turns,
            newtokens: self.newtoken_count,
            state_changes: self.state_changes,
            replay_turns: self.replay_turns,
            diverged_turns: self.diverged_turns,
            goal_games: self.goal_games,
            top10_games: self.top10_games,
            explore_games: self.explore_games,
            frontier_games: self.frontier_games,
        }
    }

    /// Fold worker counters into this search.
    pub fn add_run_counters(&mut self, counters: &RunCounters) {
        self.turns_played = self.turns_played.saturating_add(counters.turns);
        self.useful_turns = self.useful_turns.saturating_add(counters.useful_turns);
        self.state_turns = self.state_turns.saturating_add(counters.state_turns);
        self.newtoken_count = self.newtoken_count.saturating_add(counters.newtokens);
        self.state_changes = self.state_changes.saturating_add(counters.state_changes);
        self.replay_turns = self.replay_turns.saturating_add(counters.replay_turns);
        self.diverged_turns = self.diverged_turns.saturating_add(counters.diverged_turns);
        self.goal_games = self.goal_games.saturating_add(counters.goal_games);
        self.top10_games = self.top10_games.saturating_add(counters.top10_games);
        self.explore_games = self.explore_games.saturating_add(counters.explore_games);
        self.frontier_games = self.frontier_games.saturating_add(counters.frontier_games);
    }

    // --- checkpoint plumbing (checkpoint.rs uses these) ---

    pub fn stat_ids(&self) -> Vec<TokenId> {
        self.stats.keys().copied().collect()
    }

    pub fn stat(&self, id: TokenId) -> &Stat {
        &self.stats[&id]
    }

    pub fn stabletok_ids(&self) -> &[TokenId] {
        &self.stableorder
    }

    pub fn set_stats(&mut self, stats: HashMap<TokenId, Stat>) {
        self.stats = stats;
    }

    pub fn restore_stable(&mut self, id: TokenId) {
        if self.stabletoks.insert(id) {
            self.stableorder.push(id);
        }
    }

    /// Rebuild the goal queue after loading a checkpoint.
    pub fn reindex(&mut self) {
        self.recomputevmtoks();
        let ids = self.stat_ids();
        for id in ids {
            self.updatetokfreq(id);
        }
    }

    // ---------------------------------------------------------------- games

    /// A fresh machine for this story, with this search's options and a seed
    /// drawn from the search RNG.
    fn machine(&mut self) -> Zmachine<TokenUI> {
        let seed = self.rng.next_u64();
        let mut options = Options::default();
        options.log_instructions = self.log_instructions;
        options.record_events = self.usetech;
        options.rand_seed = [
            seed as u32,
            (seed >> 32) as u32,
            0x5EED_1234,
            !(seed as u32),
        ];
        Zmachine::new(self.story.clone(), TokenUI::new(), options)
    }

    pub fn play_game(&mut self) -> GameEnd {
        self.newgame();

        let mut zm = self.machine();
        self.learn_labels(&zm);

        // Banner and first room description: no `turncmd` yet, so nothing is
        // recorded (search.js: `if (turncmd === null) return`).
        match run_to_input(&mut zm) {
            Until::Input => {}
            Until::Ended | Until::Stuck => {
                self.endgame();
                return GameEnd::Quit;
            }
        }
        zm.ui.drain_prints();
        zm.drain_events();

        loop {
            // Standing at a prompt: fold the previous turn into the tables.
            self.committurn();

            // Out of turns: ask for SCORE once, then stop on the next prompt.
            if self.numturns == self.game_turns && self.prescoretok.is_some() {
                self.turncmd = None;
                self.numturns += 1;
                zm.handle_input("SCORE".to_string());
            } else if self.numturns > self.game_turns {
                self.endgame();
                return GameEnd::Turns;
            } else {
                self.makevocab(&zm);
                if self.usevmhack {
                    self.hackvm(&mut zm);
                }
                let cmd = self.choose_command();
                if self.verbose {
                    println!(
                        "CMD {} {} {}",
                        self.numturns,
                        cmd,
                        match (self.turnisreplay, self.replaydiverged) {
                            (true, _) => "(replay)",
                            (_, true) => "(diverged)",
                            _ => "",
                        }
                    );
                }
                zm.handle_input(cmd);
            }

            match run_to_input(&mut zm) {
                Until::Input => self.collect_turn(&mut zm),
                Until::Ended => {
                    // Tokens printed on the way out are still real; there is
                    // just no prompt left to commit them at.
                    self.collect_turn(&mut zm);
                    self.endgame();
                    return GameEnd::Quit;
                }
                Until::Stuck => {
                    self.collect_turn(&mut zm);
                    self.endgame();
                    return GameEnd::Stuck;
                }
            }
        }
    }

    /// Feed a fixed list of commands to a fresh machine and report the tokens
    /// each turn produced. Used to compare against a `search.js` trace: the
    /// commands are the same, so any difference is in the VM or the tokenizer.
    pub fn replay_commands(&mut self, commands: &[String]) -> Vec<(isize, String, Vec<String>)> {
        self.newgame();
        let mut zm = self.machine();
        let mut out = Vec::new();

        match run_to_input(&mut zm) {
            Until::Input => {}
            _ => return out,
        }
        zm.ui.drain_prints();
        zm.drain_events();
        self.makevocab(&zm);

        for (index, cmd) in commands.iter().enumerate() {
            self.turncmd = Some(cmd.clone());
            self.turntoks.clear();
            self.turntokseen.clear();
            zm.handle_input(cmd.clone());
            match run_to_input(&mut zm) {
                Until::Input | Until::Ended | Until::Stuck => {}
            }
            for chunk in zm.ui.drain_prints() {
                self.logprint(&chunk);
            }
            for event in zm.drain_events() {
                self.apply_event(event);
            }

            let mut names: Vec<String> = self
                .turntoks
                .iter()
                .map(|&id| self.tokens.name(id).to_string())
                .collect();
            names.sort();
            out.push((index as isize, cmd.clone(), names));
        }

        out
    }

    fn newgame(&mut self) {
        self.playtoks.clear();
        self.playvocab.clear();
        self.playvocabset.clear();
        self.turntoks.clear();
        self.turntokseen.clear();
        self.numturns = -1;
        self.turncmd = None;
        self.expectedtoks = None;
        self.replaydiverged = false;
        self.turnisreplay = false;
        self.nexttokisscore = false;
        self.curscore = 0;
        self.goalmet = false;
        self.hacks.clear();
        self.playthru = Playthrough::new();
        self.room.clear();
        self.score.clear();
        self.game_turns = self.maxturns;
        self.turn_moves.clear();
        self.turn_room_changed = false;
        self.replay_credits = 0;

        // Pick a goal from the ten most attractive tokens, except for the
        // exploration quota: some games deliberately target a token outside
        // the top ten so the queue cannot be monopolised by a few hard goals.
        let candidates: Vec<TokenId> = self
            .tokfreq
            .iter()
            .take(10)
            .map(|s| s.token)
            .filter(|&id| !self.is_ambient(id))
            .collect();
        let explore = !candidates.is_empty() && self.rng.chance(self.prob_explore);
        let explored = if explore {
            self.exploration_goal(&candidates)
        } else {
            None
        };
        let used_explore = explored.is_some();
        let frontier = if explored.is_none() && self.rng.chance(self.prob_frontier) {
            self.frontier_goal(&candidates)
        } else {
            None
        };
        let used_frontier = frontier.is_some();
        self.goaltok = match (explored, frontier) {
            (Some(goal), _) => Some(goal),
            (_, Some(goal)) => Some(goal),
            (None, None) if candidates.is_empty() => None,
            (None, None) => Some(candidates[self.rng.below(candidates.len())]),
        };

        if self.goaltok.is_some() {
            self.goal_games += 1;
            if used_explore {
                self.explore_games += 1;
            } else if used_frontier {
                self.frontier_games += 1;
            } else {
                self.top10_games += 1;
            }
        }

        if let Some(goal) = self.goaltok {
            if let Some(rec) = self.stats.get_mut(&goal) {
                rec.goalruns += 1;
                rec.score_tries += 1;
            }
            self.updatetokfreq(goal);

            // A goal discovered deep into a long run cannot be reached in a
            // normal game. Give this game enough turns for its known route,
            // plus some room to explore beyond it. The cap keeps one very deep
            // token from making every game arbitrarily expensive.
            let first = self.stats.get(&goal).map(|s| s.first).unwrap_or(0) as isize;
            let path = self
                .stats
                .get(&goal)
                .and_then(|s| s.best.as_ref())
                .map(|b| b.len() as isize)
                .unwrap_or(0);
            let reach = first.max(path);
            if reach > self.maxturns {
                let stretch = self.maxturns / TURN_STRETCH_FRACTION.max(1);
                let cap = self.maxturns.saturating_mul(4);
                self.game_turns = self.maxturns.max((reach + stretch).min(cap));
            }
        }
    }

    /// Pick a least-tried token, preferring earlier and then alphabetically
    /// first ones so the choice is stable across processes. Tokens in
    /// `exclude` (the normal top ten) and stable tokens are skipped.
    fn exploration_goal(&mut self, exclude: &[TokenId]) -> Option<TokenId> {
        let mut pool: Vec<(u64, usize, TokenId)> = self
            .stats
            .iter()
            .filter(|(id, stat)| {
                stat.first > 0
                    && stat.first < FIRST_UNKNOWN
                    && !self.stabletoks.contains(id)
                    && !exclude.contains(id)
                    && !self.is_ambient(**id)
            })
            .map(|(&id, stat)| (stat.goalruns, stat.first, id))
            .collect();
        if pool.is_empty() {
            return None;
        }
        pool.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| self.tokens.name(a.2).cmp(self.tokens.name(b.2)))
        });
        pool.truncate(EXPLORE_POOL);
        Some(pool[self.rng.below(pool.len())].2)
    }

    /// Pick a deep token with a known route that has never been reproduced as
    /// a goal. This is the complement of `exploration_goal`: exploration tries
    /// things never attempted, the frontier retries the far edge of what is
    /// already known. Without it, the linear `first` penalty in the priority
    /// formula can keep a late-game goal out of the queue forever.
    fn frontier_goal(&mut self, exclude: &[TokenId]) -> Option<TokenId> {
        let mut pool: Vec<(usize, u64, TokenId)> = self
            .stats
            .iter()
            .filter(|(id, stat)| {
                stat.first > 0
                    && stat.first < FIRST_UNKNOWN
                    && stat.goalsucc == 0
                    && stat.best.as_ref().is_some_and(|b| !b.is_empty())
                    && !self.stabletoks.contains(id)
                    && !exclude.contains(id)
                    && !self.is_ambient(**id)
            })
            .map(|(&id, stat)| (stat.first, stat.goalruns, id))
            .collect();
        if pool.is_empty() {
            return None;
        }
        // Deepest first; ties go to the least-attempted, then name for stable
        // output across processes.
        pool.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| self.tokens.name(a.2).cmp(self.tokens.name(b.2)))
        });
        pool.truncate(FRONTIER_POOL);
        Some(pool[self.rng.below(pool.len())].2)
    }

    fn endgame(&mut self) {
        if let Some(goal) = self.goaltok {
            if !self.goalmet {
                if let Some(rec) = self.stats.get_mut(&goal) {
                    rec.stablecount = (rec.stablecount - 2).max(0);
                }
            }
        }

        self.numplays += 1;
        if self.numplays % 10_000 == 0 {
            if let Some(path) = self.checkpoint_path.clone() {
                self.info(format!("CHECKPOINT {}", self.numplays));
                if let Err(e) = self.save_checkpoint(&path) {
                    println!("ERROR writing checkpoint: {}", e);
                }
            }
        }
    }

    // ------------------------------------------------------------ tokenizing

    /// Drain the turn's print calls and VM events into tokens.
    fn collect_turn(&mut self, zm: &mut Zmachine<TokenUI>) {
        for chunk in zm.ui.drain_prints() {
            self.logprint(&chunk);
        }
        for event in zm.drain_events() {
            self.apply_event(event);
        }
        if let Some((room, score)) = zm.ui.status() {
            let room_changed = !self.room.is_empty() && room != self.room;
            if room_changed {
                self.turn_room_changed = true;
                self.learn_player_from_move(room);
            }
            self.room.clear();
            self.room.push_str(room);
            self.score.clear();
            self.score.push_str(score);
        }
        self.report_progress();
    }

    /// If the object table did not name the player, infer it from the room
    /// change: the mover whose destination names the new status-bar room is
    /// the avatar. Only does anything while the player is still unknown.
    fn learn_player_from_move(&mut self, room: &str) {
        if self.player_object.is_some() || self.turn_moves.is_empty() {
            return;
        }
        let mut candidate = None;
        for &(object, parent) in &self.turn_moves {
            let name = match self.labels.plain_object(parent) {
                Some(name) => name,
                None => continue,
            };
            if name.eq_ignore_ascii_case(room.trim()) {
                candidate = Some(object);
                break;
            }
        }
        if let Some(player) = candidate {
            self.player_object = Some(player);
            self.reindex();
        }
    }

    /// Hand this turn to the live display, if one is attached. Token names are
    /// only materialized when someone is watching.
    fn report_progress(&self) {
        let callback = match &self.progress {
            Some(callback) => Arc::clone(callback),
            None => return,
        };
        let goal = self
            .goaltok
            .map(|id| self.token_label(id))
            .unwrap_or_default();
        let tokens = self
            .turntoks
            .iter()
            .map(|&id| self.token_label(id))
            .collect();
        callback(
            self,
            Progress {
                turn: self.numturns,
                goal,
                cmd: self.turncmd.clone().unwrap_or_default(),
                tokens,
                room: self.room.clone(),
                score: self.score.clone(),
            },
        );
    }

    fn apply_event(&mut self, event: VmEvent) {
        match event {
            VmEvent::Move { object, parent } => {
                self.turn_moves.push((object, parent));
                self.setstate(format!("@mv_{}", object), parent);
            }
            VmEvent::Store { variable, value } => {
                if self.record_stores {
                    self.setstate(format!("@store_{}", variable), value);
                }
            }
            VmEvent::PutProp {
                object, property, ..
            } => {
                self.setstate(format!("@pp_{}_{}", object, property), 1);
            }
            VmEvent::Flag { object, flag, set } => {
                self.setstate(format!("@f_{}_{}", object, flag), set as u16);
            }
            // Not logged by jszm's search hook; see `record_stores`.
            VmEvent::GetProp { .. } | VmEvent::Random { .. } => {}
        }
    }

    fn setstate(&mut self, key: String, value: u16) {
        // `usevmstate` is off in search.js, so this only ever logs a token.
        if self.usetech {
            self.state_changes += 1;
            self.logtoken(&format!("{}_{}", key, value));
        }
    }

    /// `game.print`: whole printed chunk becomes one token, unless it is short,
    /// a dictionary word, or numeric -- in which case it may be the score.
    fn logprint(&mut self, x: &str) {
        if self.usewords {
            let len = x.chars().count();
            let numeric = int_prefix(x).is_some();
            let is_vocab = self
                .tokens
                .get(x)
                .is_some_and(|id| self.vocabdict.contains(&id));

            if len >= 3 && !is_vocab && !numeric {
                let tok: String = x
                    .chars()
                    .take(self.wordtoklen)
                    .collect::<String>()
                    .trim()
                    .to_string();
                self.logtoken(&tok);
                // The score comes right after a "Your score is" token; any
                // other text token cancels the expectation.
                self.nexttokisscore = self.prescoretok.as_deref() == Some(tok.as_str());
            } else if self.nexttokisscore {
                self.nexttokisscore = false;
                if let Some(score) = int_prefix(x) {
                    self.curscore = score;
                    self.note_score();
                }
            }
        }

        // Words in the output that the game also knows become command words.
        for word in alpha_runs(x) {
            if word.chars().count() < 3 {
                continue;
            }
            let truncated: String = word.to_ascii_lowercase().chars().take(6).collect();
            if let Some(id) = self.tokens.get(&truncated) {
                if self.vocabdict.contains(&id) && self.playvocabset.insert(id) {
                    self.playvocab.push(id);
                }
            }
        }
    }

    fn note_score(&mut self) {
        let score = self.curscore;
        let goal = match self.goaltok {
            Some(g) => g,
            None => return,
        };
        let better = match self.stats.get(&goal) {
            Some(rec) => score > rec.maxscore,
            None => false,
        };
        if better {
            if let Some(rec) = self.stats.get_mut(&goal) {
                rec.maxscore = score;
                rec.score_tries = 0;
            }
            self.info(format!("SCORE {} {}", self.token_label(goal), score));
            self.updatetokfreq(goal);
        }
    }

    fn logtoken(&mut self, text: &str) {
        let id = self.tokens.intern(text).0;
        self.addtoken(id);
    }

    fn addtoken(&mut self, id: TokenId) {
        let cmd = match &self.turncmd {
            Some(cmd) => cmd.clone(),
            None => return, // don't record tokens before the 1st turn
        };

        if !self.stats.contains_key(&id) {
            self.stats.insert(id, Stat::new(cmd.clone()));
            self.newtoken_count += 1;
            let name = self.token_label(id);
            self.info(format!("NEWTOKEN {} {}", name, cmd));
        }

        if self.turntokseen.insert(id) {
            self.turntoks.push(id);
        }
    }

    // -------------------------------------------------------------- endings

    fn metgoal(&mut self) {
        if self.goalmet {
            return;
        }
        let goal = match self.goaltok {
            Some(g) => g,
            None => return,
        };
        if !self.stats.contains_key(&goal) {
            return;
        }
        self.credit_success(goal);
        self.goalmet = true;
    }

    /// Record one verified success for `id` and promote or demote it once the
    /// stable threshold is crossed. Shared by the chosen goal and by the
    /// positive credit a verified replay gives to goals on its prefix.
    fn credit_success(&mut self, id: TokenId) {
        let stablecount = match self.stats.get_mut(&id) {
            Some(rec) => {
                rec.goalsucc += 1;
                rec.stablecount += 1;
                rec.stablecount
            }
            None => return,
        };
        let first = self.stats[&id].first;
        // A token first seen late needs more successes before it counts as
        // stable; early tokens settle quickly.
        let thresh = self.stablethresh as f64 * (first as f64 + 1.0);

        if stablecount as f64 >= thresh {
            self.makestable(id);
        } else if (stablecount as f64) < thresh / 2.0 {
            self.makeunstable(id);
        }
        // `goalsucc` is part of the priority, but a success that does not yet
        // cross the stability threshold used to leave the queue entry stale.
        self.updatetokfreq(id);
    }

    /// A verified replay precedes the goal: every token first discovered on one
    /// of its turns was reproduced too, so one successful run can retire many
    /// goals. Only intentional replay turns count, and only a bounded number
    /// per game, so cheap parser responses cannot be inflated.
    fn credit_replay_bundle(&mut self) {
        if self.replaydiverged || self.replay_credits >= MAX_REPLAY_CREDITS {
            return;
        }
        let goal = match self.goaltok {
            Some(goal) => goal,
            None => return,
        };
        let reached = self.numturns.max(0) as usize;
        let replays: Vec<Vec<TokenId>> = self
            .playthru
            .turns
            .iter()
            .filter(|turn| turn.replay)
            .map(|turn| turn.toks.clone())
            .collect();
        for toks in replays {
            for token in toks {
                if token == goal || self.replay_credits >= MAX_REPLAY_CREDITS {
                    continue;
                }
                let eligible = match self.stats.get(&token) {
                    Some(stat) => stat.first > 0 && stat.first <= reached,
                    None => false,
                };
                if eligible {
                    self.credit_success(token);
                    self.replay_credits += 1;
                }
            }
        }
    }

    fn makestable(&mut self, id: TokenId) {
        if self.stabletoks.insert(id) {
            self.stableorder.push(id);
            self.recomputevmtoks();
            if self.stats.contains_key(&id) {
                self.updatetokfreq(id);
            }
            self.info(format!("STABLE {}", self.token_label(id)));
        }
    }

    fn makeunstable(&mut self, id: TokenId) {
        if self.stabletoks.remove(&id) {
            self.stableorder.retain(|&t| t != id);
            self.recomputevmtoks();
            if self.stats.contains_key(&id) {
                self.updatetokfreq(id);
            }
            self.info(format!("UNSTABLE {}", self.token_label(id)));
        }
    }

    fn recomputevmtoks(&mut self) {
        self.vmtoks = self
            .stableorder
            .iter()
            .copied()
            .filter(|&id| self.tokens.name(id).starts_with("@mv_") && !self.is_ambient(id))
            .collect();
    }

    /// Goal priority. Lower is more attractive:
    ///
    /// ```text
    /// first * w_distance + ln(1+goalruns) * w_untried
    ///   - ln(1+goalsucc) * w_success
    ///   + fails * failure_rate * w_fail
    ///   + stable * w_stable - effective_score
    /// ```
    ///
    /// `fails` is `goalruns - goalsucc` and `failure_rate` is
    /// `fails / (goalruns + 1)`: repeated failures should push a goal down, not
    /// make it more attractive, but a goal with a few successes is not punished
    /// like one that has never worked. `effective_score` decays with the number
    /// of attempts since `maxscore` last improved, so one lucky scored run
    /// cannot pin a token to the top of the queue forever.
    fn updatetokfreq(&mut self, id: TokenId) {
        // Ambient state (NPC moves) is not actionable by a command, so it
        // never belongs in the goal queue. Drop any stale entry.
        if self.is_ambient(id) {
            let old = self
                .stats
                .get_mut(&id)
                .and_then(|stat| stat.priority.take());
            if let Some(old) = old {
                self.tokfreq.remove(&old);
            }
            return;
        }

        let stable = self.stabletoks.contains(&id);

        let (priority, old) = match self.stats.get_mut(&id) {
            Some(stat) => {
                let old = stat.priority.take();
                let fails = stat.goalruns.saturating_sub(stat.goalsucc);
                let failure_rate = fails as f64 / (stat.goalruns as f64 + 1.0);
                let failure_penalty = fails as f64 * failure_rate * self.w_fail;
                let score_bonus = if stat.maxscore > 0 {
                    stat.maxscore as f64 * self.scoremult
                        / (1.0 + stat.score_tries as f64 / self.score_decay.max(1.0))
                } else {
                    0.0
                };
                let value = stat.first as f64 * self.w_distance
                    + (stat.goalruns as f64 + 1.0).ln() * self.w_untried
                    - (stat.goalsucc as f64 + 1.0).ln() * self.w_success
                    + failure_penalty
                    + if stable { self.w_stable } else { 0.0 }
                    - score_bonus;
                // Tokens from the very first turn are uninformative; leave them
                // out of the queue entirely.
                let priority = if stat.first > 0 {
                    Some(Score {
                        value,
                        token: id,
                        name: self.tokens.shared(id),
                    })
                } else {
                    None
                };
                (priority, old)
            }
            None => return,
        };

        if let Some(old) = old {
            self.tokfreq.remove(&old);
        }
        if let Some(score) = priority {
            self.tokfreq.insert(score.clone());
            if let Some(stat) = self.stats.get_mut(&id) {
                stat.priority = Some(score);
            }
        }
    }

    // ------------------------------------------------------------ committing

    /// `committurn` from search.js: close out the turn that just ran.
    fn committurn(&mut self) {
        if self.numturns >= 0 {
            let toks = self.turntoks.clone();
            let cmd = self.turncmd.clone().unwrap_or_default();
            self.playthru.addturn(cmd, toks, self.turnisreplay);
        }

        // Did a sequential replay reproduce the tokens recorded for its turn?
        if self.turnisreplay {
            self.replay_turns += 1;
            if let Some(expected) = &self.expectedtoks {
                if !sametokens(&self.turntoks, expected) {
                    self.replaydiverged = true;
                    self.diverged_turns += 1;
                    self.info(format!(
                        "DIVERGE {} {} expected {} got {}",
                        self.numturns,
                        self.turncmd.clone().unwrap_or_default(),
                        expected.len(),
                        self.turntoks.len()
                    ));
                }
            }
        }
        self.expectedtoks = None;

        let goal_hit = self
            .goaltok
            .is_some_and(|goal| self.turntokseen.contains(&goal));
        if goal_hit {
            self.metgoal();
            if self.turnisreplay {
                self.credit_replay_bundle();
            }
        }

        if self.turncmd.is_some() {
            self.turns_played += 1;
            let index = self.playthru.len() as isize - 1;
            // A parser error also prints text, so "produced a token" is not
            // enough: a command is useful when it changed state, changed rooms,
            // or revealed text for the first time.
            let state_token = self
                .turntoks
                .iter()
                .any(|&tok| self.tokens.name(tok).starts_with('@'));
            let new_token = self.turntoks.iter().any(|&tok| {
                self.stats
                    .get(&tok)
                    .is_some_and(|stat| stat.first > index as usize)
            });
            if state_token {
                self.state_turns += 1;
            }
            if self.turn_room_changed || state_token || new_token {
                self.useful_turns += 1;
                if let Some(cmd) = self.turncmd.clone() {
                    self.record_template(&cmd);
                }
            }

            for tok in self.turntoks.clone() {
                let mut reduced = false;
                if let Some(stat) = self.stats.get_mut(&tok) {
                    stat.count += 1;
                    if index < stat.first as isize {
                        reduced = true;
                    }
                }
                if reduced {
                    let cmd = self.turncmd.clone().unwrap_or_default();
                    // Snapshot the path: playthru keeps growing, this must not.
                    let best = if self.numturns == 0 {
                        None
                    } else {
                        Some(self.playthru.turns.clone())
                    };
                    if let Some(stat) = self.stats.get_mut(&tok) {
                        if self.verbose {
                            let succ = stat.goalsucc;
                            let runs = stat.goalruns;
                            println!(
                                "REDUCE {} {} < {} ({}/{}/{})",
                                // Label via the field: `token_label` would
                                // borrow all of `self` while `stat` is live.
                                self.labels.label(self.tokens.name(tok)),
                                index,
                                stat.first,
                                succ,
                                runs,
                                stat.count
                            );
                        }
                        stat.best = best;
                        stat.first = index as usize;
                        stat.cmds.push(cmd);
                    }
                }
                self.updatetokfreq(tok);
            }

            for tok in &self.turntoks {
                self.playtoks.insert(*tok);
            }
        }

        self.turntoks.clear();
        self.turntokseen.clear();
        self.turn_moves.clear();
        self.turn_room_changed = false;
        self.numturns += 1;
    }

    /// Remember a command that changed something. Commands are stored as word
    /// vectors so a mutation can replace one slot without knowing grammar.
    fn record_template(&mut self, cmd: &str) {
        let words: Vec<String> = cmd.split_whitespace().map(|w| w.to_string()).collect();
        if words.is_empty() || words.len() > 4 {
            return;
        }
        let key = words.join(" ");
        if self.cmdtemplateset.insert(key) {
            if self.cmdtemplates.len() >= TEMPLATE_POOL {
                // Replace an arbitrary entry; the reservoir does not need to be
                // uniform, only to keep following new discoveries.
                let index = self.rng.below(self.cmdtemplates.len());
                let old = self.cmdtemplates.swap_remove(index);
                self.cmdtemplateset.remove(&old.join(" "));
            }
            self.cmdtemplates.push(words);
        }
    }

    // ------------------------------------------------------------- commands

    fn makevocab(&mut self, zm: &Zmachine<TokenUI>) {
        if self.vocab.is_some() {
            return;
        }
        let mut words: Vec<TokenId> = Vec::new();
        for word in zm.get_dictionary() {
            if !word.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_') {
                continue;
            }
            if IGNORED_WORDS.contains(&word.as_str()) {
                continue;
            }
            let id = self.tokens.intern(&word).0;
            self.vocabdict.insert(id);
            words.push(id);
        }
        self.vocab = Some(words);
    }

    fn getrandomcmd(&mut self) -> String {
        // Mined commands first: they tend to be grammatical, which is the
        // difference between a parser error and an actual state change.
        if !self.cmdtemplates.is_empty() && self.rng.chance(self.prob_template) {
            if let Some(cmd) = self.template_cmd() {
                return cmd;
            }
        }

        loop {
            let mut cmd = String::new();
            for i in 0..2 {
                if i > 0 {
                    cmd.push(' ');
                }
                let word = match self.sample_word() {
                    Some(word) => word,
                    // No dictionary: nothing sensible to type.
                    None => return "look".to_string(),
                };
                cmd.push_str(self.tokens.name(word));

                if self.rng.chance(self.prob_end) {
                    break;
                }
            }

            if !IGNORED_WORDS.contains(&cmd.as_str()) {
                return cmd;
            }
        }
    }

    /// One command word, preferring words the game has actually understood.
    fn sample_word(&mut self) -> Option<TokenId> {
        let recent = !self.playvocab.is_empty() && self.rng.chance(self.prob_vocab);
        if recent {
            let i = self.rng.below(self.playvocab.len());
            return Some(self.playvocab[i]);
        }
        match &self.vocab {
            Some(vocab) if !vocab.is_empty() => {
                let i = self.rng.below(vocab.len());
                Some(vocab[i])
            }
            _ => None,
        }
    }

    /// Take a command that worked and mutate one slot. This is the cheap half
    /// of a contextual command model: no grammar, just the shapes the story has
    /// already accepted.
    fn template_cmd(&mut self) -> Option<String> {
        if self.cmdtemplates.is_empty() {
            return None;
        }
        let index = self.rng.below(self.cmdtemplates.len());
        let mut words = self.cmdtemplates[index].clone();

        if self.rng.chance(0.75) && !words.is_empty() {
            let slot = self.rng.below(words.len());
            if let Some(word) = self.sample_word() {
                words[slot] = self.tokens.name(word).to_string();
            }
        }
        if words.len() < 4 && self.rng.chance(0.15) {
            if let Some(word) = self.sample_word() {
                words.push(self.tokens.name(word).to_string());
            }
        } else if words.len() > 1 && self.rng.chance(0.1) {
            words.pop();
        }

        let cmd = words.join(" ");
        if IGNORED_WORDS.contains(&cmd.as_str()) {
            None
        } else {
            Some(cmd)
        }
    }

    /// `game.read`: replay logic first, random command as the fallback.
    fn choose_command(&mut self) -> String {
        self.turnisreplay = false;
        self.expectedtoks = None;

        let mut cmd: Option<String> = None;

        let goal = match self.goaltok {
            Some(g) => g,
            None => {
                let cmd = self.getrandomcmd();
                self.turncmd = Some(cmd.clone());
                return cmd;
            }
        };

        let (goalruns, goalsucc, first, best) = match self.stats.get(&goal) {
            Some(rec) => (rec.goalruns, rec.goalsucc, rec.first, rec.best.clone()),
            None => (0, 0, FIRST_UNKNOWN, None),
        };

        if let Some(best) = best {
            let turn = best.get(self.numturns.max(0) as usize).cloned();
            // A path found by a random game is still a path: replay it in
            // order when the goal is far enough away that retrying just the
            // final command cannot work. `turn.replay` only says the snapshot
            // was itself recorded during a replay, not that the path is valid.
            let long_path = first as isize >= LONG_PATH_TURNS;

            if !self.replaydiverged && long_path && turn.is_some() {
                let turn = turn.unwrap();
                cmd = Some(turn.cmd);
                self.turnisreplay = true;
                self.expectedtoks = Some(turn.toks);
            } else if !self.replaydiverged && goalruns <= 2 && turn.is_some() {
                let turn = turn.unwrap();
                if turn.replay {
                    // Stick to the recorded replay verbatim.
                    cmd = Some(turn.cmd);
                    self.turnisreplay = true;
                    self.expectedtoks = Some(turn.toks);
                } else {
                    // On the first couple of attempts, just retry the command
                    // that produced the goal.
                    let index = if goalruns < 2 || first == 0 {
                        first
                    } else {
                        (first as isize - (self.numturns & 1)) as usize
                    };
                    if let Some(t) = best.get(index).or_else(|| best.get(first)) {
                        cmd = Some(t.cmd.clone());
                    }
                }
            } else if self.numturns.max(0) as usize <= first {
                let turn = turn.or_else(|| best.get(self.numturns.max(0) as usize).cloned());
                if let Some(turn) = turn {
                    // Replays get less likely as the goal is met more often.
                    let denominator = 1 + goalsucc + goalruns * (turn.replay as u64);
                    let p = (2 + (first * first) as u64) as f64 / denominator as f64;
                    let shuffle = self.replaydiverged || self.rng.chance(p);

                    if shuffle {
                        if self.rng.chance(0.75) {
                            let i = self.rng.below((first + 1).min(best.len()));
                            cmd = Some(best[i].cmd.clone());
                        }
                        // else: fall through to a random command
                    } else {
                        cmd = Some(turn.cmd.clone());
                        self.turnisreplay = true;
                        self.expectedtoks = Some(turn.toks.clone());
                    }
                }
            }
        }

        let cmd = cmd.unwrap_or_else(|| self.getrandomcmd());
        self.turncmd = Some(cmd.clone());
        cmd
    }

    /// `hackvm`: sometimes start the game with an object moved somewhere it
    /// cannot normally go, and prefix the playthrough with the path that
    /// reaches the goal token. This is what breaks the search out of logjams.
    fn hackvm(&mut self, zm: &mut Zmachine<TokenUI>) {
        let goal = match self.goaltok {
            Some(g) => g,
            None => return,
        };
        let (stablecount, first, best) = match self.stats.get(&goal) {
            Some(rec) => (rec.stablecount, rec.first, rec.best.clone()),
            None => (0, FIRST_UNKNOWN, None),
        };

        // The more stable the goal, the bolder the hack.
        let prob = stablecount as f64 * 0.1;
        if self.numturns != 0 || self.vmtoks.is_empty() || !self.rng.chance(prob) {
            return;
        }
        if !self.stabletoks.contains(&goal) {
            return;
        }

        let index = self.rng.below(self.vmtoks.len());
        let tok = self.vmtoks[index];
        let name = self.tokens.name(tok).to_string();
        let rest = match name.strip_prefix("@mv_") {
            Some(rest) => rest,
            None => return,
        };
        let mut parts = rest.split('_');
        let (x, y) = match (
            parts.next().and_then(|s| s.parse::<u16>().ok()),
            parts.next().and_then(|s| s.parse::<u16>().ok()),
        ) {
            (Some(x), Some(y)) => (x, y),
            _ => return,
        };

        // Keep it inside the object table: `mv` walks the sibling chain, and a
        // made-up object number would walk off the end of it.
        let count = zm.object_count();
        if x >= 1 && x <= count && y <= count {
            zm.mv(x, y);
            // `name` is the raw token (the parse above needs it); the log is
            // display, so it gets the readable form.
            self.info(format!("HACK {}", self.token_label(tok)));
        }
        self.hacks.push(tok);

        // Reuse the shortest known path to this token as a head start.
        if let Some(best) = best {
            let last = first.min(best.len().saturating_sub(1));
            for turn in &best[..=last] {
                self.playthru
                    .addturn(turn.cmd.clone(), turn.toks.clone(), true);
            }
        }
    }

    fn info(&mut self, message: String) {
        if self.verbose {
            println!("{}", message);
        }
        if self.eventbuf.len() >= EVENT_BUFFER {
            self.eventbuf.pop_front();
        }
        self.eventbuf.push_back(message);
    }

    /// Events emitted since the last call, for a display or a batch report.
    pub fn take_events(&mut self) -> Vec<String> {
        self.eventbuf.drain(..).collect()
    }

    /// The goal this game is currently aiming at.
    pub fn current_goal(&self) -> String {
        self.goaltok
            .map(|id| self.token_label(id))
            .unwrap_or_default()
    }

    /// The command sent most recently.
    pub fn current_cmd(&self) -> String {
        self.turncmd.clone().unwrap_or_else(|| {
            // Between games `turncmd` is empty, and the last recorded turn is
            // the synthetic SCORE one (also empty); the display wants the last
            // command that actually went to the story.
            self.playthru
                .turns
                .iter()
                .rev()
                .find(|turn| !turn.cmd.is_empty())
                .map(|turn| turn.cmd.clone())
                .unwrap_or_default()
        })
    }
}

/// Parse an `@mv_<object>_<parent>` token's two object numbers.
fn move_parts(name: &str) -> Option<(u16, u16)> {
    let rest = name.strip_prefix("@mv_")?;
    let (object, parent) = rest.split_once('_')?;
    Some((object.parse().ok()?, parent.parse().ok()?))
}

fn sametokens(actual: &[TokenId], expected: &[TokenId]) -> bool {
    if actual.len() != expected.len() {
        return false;
    }
    let set: HashSet<TokenId> = actual.iter().copied().collect();
    expected.iter().all(|t| set.contains(t))
}

enum Until {
    Input,
    Ended,
    Stuck,
}

/// Run until the game wants input.
fn run_to_input(zm: &mut Zmachine<TokenUI>) -> Until {
    loop {
        match zm.step_with_budget(MAX_INSTRUCTIONS_PER_INPUT) {
            None => return Until::Stuck,
            Some(Step::Done) => return Until::Ended,
            Some(Step::ReadChar) => zm.handle_read_char(ZChar::RETURN),
            // A save with no file name given behaves like "no thanks".
            Some(Step::Save(_)) => zm.handle_save_result(false),
            Some(Step::Restore) => zm.handle_restore_result(),
            Some(Step::ReadLine) => return Until::Input,
        }
    }
}

/// `parseInt(x)` for base 10: leading space, optional sign, then digits.
fn int_prefix(text: &str) -> Option<i64> {
    let trimmed = text.trim_start();
    let mut chars = trimmed.char_indices();
    let mut end = 0;
    let mut start = 0;
    match chars.next() {
        Some((_, c)) if c == '+' || c == '-' => start = c.len_utf8(),
        Some(_) => {}
        None => return None,
    }
    for (i, c) in trimmed[start..].char_indices() {
        if c.is_ascii_digit() {
            end = start + i + c.len_utf8();
        } else {
            break;
        }
    }
    if end == start {
        return None;
    }
    trimmed[..end].parse::<i64>().ok()
}

/// Runs of ASCII letters, the way `x.split(/[^a-z]/i)` yields them.
fn alpha_runs(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !c.is_ascii_alphabetic())
        .filter(|w| !w.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int_prefix_matches_parse_int() {
        assert_eq!(int_prefix("12"), Some(12));
        assert_eq!(int_prefix("  12 "), Some(12));
        assert_eq!(int_prefix("12abc"), Some(12));
        assert_eq!(int_prefix("-3"), Some(-3));
        assert_eq!(int_prefix("+7"), Some(7));
        assert_eq!(int_prefix("abc"), None);
        assert_eq!(int_prefix(""), None);
        assert_eq!(int_prefix("0"), Some(0));
    }

    #[test]
    fn alpha_runs_split_on_non_letters() {
        let runs: Vec<&str> = alpha_runs("take the lamp, 2x").collect();
        assert_eq!(runs, vec!["take", "the", "lamp", "x"]);
    }

    #[test]
    fn ambient_moves_never_become_goals() {
        let mut search = Search::new(Vec::new(), 1, 10, false);
        search.labels = Labels::from_tree("├── you (30)\n├── Thief (112)\n");
        search.player_object = search.labels.player();
        assert_eq!(search.player_object, Some(30));

        let mut stats = HashMap::new();
        for (name, first) in [
            ("@mv_30_28", 1),
            ("@mv_112_57", 1),
            ("@mv_175_30", 1),
            ("printed text", 2),
        ] {
            let id = search.tokens.intern(name).0;
            let mut stat = Stat::new("x".to_string());
            stat.first = first;
            stats.insert(id, stat);
        }
        search.set_stats(stats);
        search.reindex();

        let names: Vec<String> = search
            .goal_queue()
            .into_iter()
            .map(|(_, id)| search.token_name(id).to_string())
            .collect();
        assert!(names.contains(&"@mv_30_28".to_string()), "player move kept");
        assert!(
            !names.contains(&"@mv_112_57".to_string()),
            "thief move excluded"
        );
        assert!(names.contains(&"printed text".to_string()), "text kept");
        assert!(
            names.contains(&"@mv_175_30".to_string()),
            "item taken into inventory kept"
        );
        let thief = search.tokens.get("@mv_112_57").unwrap();
        assert!(search.is_ambient(thief));
    }

    #[test]
    fn exploration_skips_the_top_ten() {
        let mut search = Search::new(Vec::new(), 1, 10, false);
        let mut stats = HashMap::new();
        let mut top = Vec::new();
        for i in 0..10 {
            let id = search.tokens.intern(&format!("top{}", i)).0;
            let mut stat = Stat::new("x".to_string());
            stat.first = 1;
            stat.goalruns = i as u64;
            stat.goalsucc = i as u64;
            stats.insert(id, stat);
            top.push(id);
        }
        let outside = search.tokens.intern("outside").0;
        let mut stat = Stat::new("x".to_string());
        stat.first = 50;
        stats.insert(outside, stat);
        search.set_stats(stats);

        for _ in 0..20 {
            assert_eq!(search.exploration_goal(&top), Some(outside));
        }
    }
}

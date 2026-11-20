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

use crate::rng::Rng;
use crate::stat::{Playthrough, Score, Stat, FIRST_UNKNOWN};
use crate::token::{Interner, TokenId};
use crate::ui::TokenUI;

/// How long the VM may run without asking for input before we give up on the
/// run. `search.js` would simply hang here; a fuzzer cannot afford that.
const MAX_INSTRUCTIONS_PER_INPUT: usize = 200_000;

/// Words filtered out of the dictionary: they end or restart the game.
const IGNORED_WORDS: [&str; 6] = ["restor", "restar", "save", "q", "quit", "zzkjlk"];

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
#[derive(Debug, Clone)]
pub struct Progress {
    pub turn: isize,
    pub goal: String,
    pub cmd: String,
    pub tokens: Vec<String>,
}

/// Events kept for the display: enough to fill a screen, bounded so a
/// never-drained search cannot grow without limit.
const EVENT_BUFFER: usize = 512;

pub struct Search {
    story: Vec<u8>,

    // --- knobs, named after the search.js consts ---
    pub maxturns: isize,
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
    pub w_stable: f64,
    pub verbose: bool,
    pub log_instructions: bool,
    pub checkpoint_path: Option<PathBuf>,
    /// Called once per turn, if a live display is attached.
    pub progress: Option<Arc<dyn Fn(Progress) + Send + Sync>>,

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
    /// `NEWTOKEN` / `STABLE` / `HACK` / ... lines awaiting the display.
    eventbuf: VecDeque<String>,
}

impl Search {
    pub fn new(story: Vec<u8>, seed: u64, maxturns: isize, verbose: bool) -> Search {
        Search {
            story,
            maxturns,
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
            w_stable: 1000.0,
            verbose,
            log_instructions: false,
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
            eventbuf: VecDeque::new(),
        }
    }

    /// Turns the last game actually played.
    pub fn last_turns(&self) -> isize {
        self.numturns.max(0)
    }

    pub fn version(&self) -> u8 {
        *self.story.first().unwrap_or(&0)
    }

    pub fn token_count(&self) -> usize {
        self.stats.len()
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

    pub fn token_name(&self, id: TokenId) -> &str {
        self.tokens.name(id)
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
            if self.numturns == self.maxturns && self.prescoretok.is_some() {
                self.turncmd = None;
                self.numturns += 1;
                zm.handle_input("SCORE".to_string());
            } else if self.numturns > self.maxturns {
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

        // Pick a goal from the ten most attractive tokens.
        let candidates: Vec<TokenId> = self.tokfreq.iter().take(10).map(|s| s.token).collect();
        self.goaltok = if candidates.is_empty() {
            None
        } else {
            Some(candidates[self.rng.below(candidates.len())])
        };

        if let Some(goal) = self.goaltok {
            if let Some(rec) = self.stats.get_mut(&goal) {
                rec.goalruns += 1;
            }
            self.updatetokfreq(goal);
        }
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
        self.report_progress();
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
            .map(|id| self.tokens.name(id).to_string())
            .unwrap_or_default();
        let tokens = self
            .turntoks
            .iter()
            .map(|&id| self.tokens.name(id).to_string())
            .collect();
        callback(Progress {
            turn: self.numturns,
            goal,
            cmd: self.turncmd.clone().unwrap_or_default(),
            tokens,
        });
    }

    fn apply_event(&mut self, event: VmEvent) {
        match event {
            VmEvent::Move { object, parent } => {
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
            }
            self.info(format!("SCORE {} {}", self.tokens.name(goal), score));
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
            let name = self.tokens.name(id).to_string();
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

        let stablecount = match self.stats.get_mut(&goal) {
            Some(rec) => {
                rec.goalsucc += 1;
                rec.stablecount += 1;
                rec.stablecount
            }
            None => return,
        };
        let first = self.stats[&goal].first;
        // A token first seen late needs more successes before it counts as
        // stable; early tokens settle quickly.
        let thresh = self.stablethresh as f64 * (first as f64 + 1.0);

        if stablecount as f64 >= thresh {
            self.makestable(goal);
        } else if (stablecount as f64) < thresh / 2.0 {
            self.makeunstable(goal);
        }
        self.goalmet = true;
    }

    fn makestable(&mut self, id: TokenId) {
        if self.stabletoks.insert(id) {
            self.stableorder.push(id);
            self.recomputevmtoks();
            if self.stats.contains_key(&id) {
                self.updatetokfreq(id);
            }
            self.info(format!("STABLE {}", self.tokens.name(id)));
        }
    }

    fn makeunstable(&mut self, id: TokenId) {
        if self.stabletoks.remove(&id) {
            self.stableorder.retain(|&t| t != id);
            self.recomputevmtoks();
            if self.stats.contains_key(&id) {
                self.updatetokfreq(id);
            }
            self.info(format!("UNSTABLE {}", self.tokens.name(id)));
        }
    }

    fn recomputevmtoks(&mut self) {
        self.vmtoks = self
            .stableorder
            .iter()
            .copied()
            .filter(|&id| self.tokens.name(id).starts_with("@mv_"))
            .collect();
    }

    /// Goal priority. Lower is more attractive:
    ///
    /// ```text
    /// first * w_distance + ln(1+goalruns) * w_untried
    ///   - ln(1+goalsucc) * w_success + stable * w_stable - maxscore * scoremult
    /// ```
    ///
    /// Saturating the run/success counts stops one token from dominating; the
    /// score term is what makes a scored path crowd out everything else.
    fn updatetokfreq(&mut self, id: TokenId) {
        let stable = self.stabletoks.contains(&id);

        let (priority, old) = match self.stats.get_mut(&id) {
            Some(stat) => {
                let old = stat.priority.take();
                let value = stat.first as f64 * self.w_distance
                    + (stat.goalruns as f64 + 1.0).ln() * self.w_untried
                    - (stat.goalsucc as f64 + 1.0).ln() * self.w_success
                    + if stable { self.w_stable } else { 0.0 }
                    - stat.maxscore as f64 * self.scoremult;
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
            if let Some(expected) = &self.expectedtoks {
                if !sametokens(&self.turntoks, expected) {
                    self.replaydiverged = true;
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

        if let Some(goal) = self.goaltok {
            if self.turntokseen.contains(&goal) {
                self.metgoal();
            }
        }

        if self.turncmd.is_some() {
            let index = self.playthru.len() as isize - 1;

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
                                self.tokens.name(tok),
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
        self.numturns += 1;
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
        loop {
            let mut cmd = String::new();
            for i in 0..2 {
                if i > 0 {
                    cmd.push(' ');
                }

                // Prefer words we have actually seen the game understand.
                let recent = !self.playvocab.is_empty() && self.rng.chance(self.prob_vocab);
                let word = if recent {
                    let i = self.rng.below(self.playvocab.len());
                    self.playvocab[i]
                } else {
                    match &self.vocab {
                        Some(vocab) if !vocab.is_empty() => {
                            let i = self.rng.below(vocab.len());
                            vocab[i]
                        }
                        // No dictionary: nothing sensible to type.
                        _ => return "look".to_string(),
                    }
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

            if !self.replaydiverged && goalruns <= 2 && turn.is_some() {
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
            self.info(format!("HACK {}", name));
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
            .map(|id| self.tokens.name(id).to_string())
            .unwrap_or_default()
    }

    /// The command sent most recently.
    pub fn current_cmd(&self) -> String {
        self.turncmd.clone().unwrap_or_default()
    }
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
}

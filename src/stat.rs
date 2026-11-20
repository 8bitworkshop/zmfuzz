//! Per-token statistics and the playthrough log, mirroring `search.js`.

use std::cmp::Ordering;
use std::rc::Rc;

use crate::token::TokenId;

/// `first` before the token has been committed to a turn (JS uses 99999).
pub const FIRST_UNKNOWN: usize = 99_999;

/// One turn of a playthrough. `replay` marks turns that a previous run executed
/// as a sequential replay, which `search.js` uses to know how far a replay got.
#[derive(Debug, Clone)]
pub struct Turn {
    pub cmd: String,
    pub toks: Vec<TokenId>,
    pub replay: bool,
}

/// The commands of one game, in order. Shared by token records as "the shortest
/// known way to this token".
#[derive(Debug, Clone)]
pub struct Playthrough {
    pub turns: Vec<Turn>,
}

impl Playthrough {
    pub fn new() -> Playthrough {
        Playthrough { turns: Vec::new() }
    }

    pub fn addturn(&mut self, cmd: String, toks: Vec<TokenId>, replay: bool) {
        self.turns.push(Turn { cmd, toks, replay });
    }

    pub fn len(&self) -> usize {
        self.turns.len()
    }
}

/// Sort key for the "most promising goal" queue: lower is more attractive.
///
/// `search.js` uses `sorted-map` with the token as the key and the priority as
/// the value, so equal priorities keep insertion order there. Here the token
/// *name* breaks ties: ids are per-process, and a resumed search must queue the
/// same goals in the same order as the one that wrote the checkpoint.
#[derive(Debug, Clone)]
pub struct Score {
    pub value: f64,
    pub token: TokenId,
    pub name: Rc<str>,
}

impl PartialEq for Score {
    fn eq(&self, other: &Score) -> bool {
        self.value.total_cmp(&other.value) == Ordering::Equal && self.name == other.name
    }
}

impl Eq for Score {}

impl PartialOrd for Score {
    fn partial_cmp(&self, other: &Score) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Score {
    fn cmp(&self, other: &Score) -> Ordering {
        self.value
            .total_cmp(&other.value)
            .then_with(|| self.name.cmp(&other.name))
    }
}

#[derive(Debug, Clone)]
pub struct Stat {
    /// Number of turns the token has appeared in.
    pub count: u64,
    /// Number of games this token was chosen as the goal of.
    pub goalruns: u64,
    /// Number of those games where the goal was met.
    pub goalsucc: u64,
    /// Index of the earliest turn the token was seen in.
    pub first: usize,
    /// Commands that first produced the token.
    pub cmds: Vec<String>,
    /// How many times the goal has been met in a row (may go negative-ish: it
    /// saturates at 0).
    pub stablecount: i64,
    /// Best score seen while this token was the goal.
    pub maxscore: i64,
    /// Goal attempts since `maxscore` last improved. Kept as telemetry only:
    /// the score bonus decays on `goalruns`, which merges additively across
    /// workers, so a fresh zero from one worker cannot reset the clock.
    pub score_tries: u64,
    /// Snapshot of the playthrough up to `first`, once known.
    pub best: Option<Vec<Turn>>,
    /// Current position in the goal queue, so it can be removed on update.
    pub priority: Option<Score>,
}

impl Stat {
    pub fn new(cmd: String) -> Stat {
        Stat {
            count: 0,
            goalruns: 0,
            goalsucc: 0,
            first: FIRST_UNKNOWN,
            cmds: vec![cmd],
            stablecount: 0,
            maxscore: 0,
            score_tries: 0,
            best: None,
            priority: None,
        }
    }
}

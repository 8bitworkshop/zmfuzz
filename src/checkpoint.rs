//! Resumable search state, the equivalent of `search.js`'s `flatted` blob.
//!
//! The JS version serializes `{numplays, alltokstats, stabletoks}` wholesale.
//! Tokens are strings there and indices here, so the on-disk form keeps strings
//! and the search re-interns on load.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use serde_derive::{Deserialize, Serialize};

use crate::search::Search;
use crate::stat::{Stat, Turn, FIRST_UNKNOWN};
use crate::token::TokenId;

#[derive(Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub numplays: u64,
    pub stabletoks: Vec<String>,
    pub stats: Vec<PersistStat>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PersistStat {
    pub token: String,
    pub count: u64,
    pub goalruns: u64,
    pub goalsucc: u64,
    pub first: usize,
    pub cmds: Vec<String>,
    pub stablecount: i64,
    pub maxscore: i64,
    /// Goal attempts since `maxscore` last improved.
    pub score_tries: u64,
    #[serde(default)]
    pub best: Option<Vec<PersistTurn>>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PersistTurn {
    pub cmd: String,
    #[serde(default)]
    pub replay: bool,
    #[serde(default)]
    pub toks: Vec<String>,
}

impl Search {
    pub fn to_checkpoint(&self) -> Checkpoint {
        let mut stats: Vec<PersistStat> = self
            .stat_ids()
            .into_iter()
            .map(|id| {
                let stat = self.stat(id);
                PersistStat {
                    token: self.tokens.name(id).to_string(),
                    count: stat.count,
                    goalruns: stat.goalruns,
                    goalsucc: stat.goalsucc,
                    first: stat.first,
                    cmds: stat.cmds.clone(),
                    stablecount: stat.stablecount,
                    maxscore: stat.maxscore,
                    score_tries: stat.score_tries,
                    best: stat.best.as_ref().map(|turns| {
                        turns
                            .iter()
                            .map(|t| PersistTurn {
                                cmd: t.cmd.clone(),
                                replay: t.replay,
                                toks: t
                                    .toks
                                    .iter()
                                    .map(|&tok| self.tokens.name(tok).to_string())
                                    .collect(),
                            })
                            .collect()
                    }),
                }
            })
            .collect();
        stats.sort_by(|a, b| a.token.cmp(&b.token));

        Checkpoint {
            numplays: self.numplays,
            stabletoks: self
                .stabletok_ids()
                .iter()
                .copied()
                .map(|id| self.tokens.name(id).to_string())
                .collect(),
            stats,
        }
    }

    pub fn save_checkpoint(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string(&self.to_checkpoint())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        // Write then rename, like search.js: a killed run must not leave a
        // truncated checkpoint where a resumable search used to be.
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, json)?;
        fs::rename(&tmp, path)
    }

    pub fn load_checkpoint(&mut self, path: &Path) -> std::io::Result<()> {
        let data = fs::read_to_string(path)?;
        let checkpoint: Checkpoint = serde_json::from_str(&data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let loaded = self.load(checkpoint);
        if loaded == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "checkpoint contained no tokens",
            ));
        }
        Ok(())
    }

    /// Install checkpoint contents. Tokens are re-interned, so ids from an old
    /// lookup table do not need to match.
    pub fn load(&mut self, checkpoint: Checkpoint) -> usize {
        self.numplays = checkpoint.numplays;

        // The goal queue excludes ambient moves, which needs the object table.
        // Read it here so a loaded search queues the same goals as the one that
        // wrote the checkpoint, even before it plays a game.
        self.ensure_labels();

        let stable: Vec<TokenId> = checkpoint
            .stabletoks
            .iter()
            .map(|s| self.tokens.intern(s).0)
            .collect();

        let mut stats: HashMap<TokenId, Stat> = HashMap::new();
        for p in checkpoint.stats {
            let id = self.tokens.intern(&p.token).0;
            let best = p.best.map(|turns| {
                turns
                    .into_iter()
                    .map(|t| Turn {
                        cmd: t.cmd,
                        replay: t.replay,
                        toks: t.toks.iter().map(|s| self.tokens.intern(s).0).collect(),
                    })
                    .collect()
            });
            stats.insert(
                id,
                Stat {
                    // An unknown `first` would sort the token to the back of the
                    // queue; preserved as-is, sentinel included.
                    first: if p.first > FIRST_UNKNOWN {
                        FIRST_UNKNOWN
                    } else {
                        p.first
                    },
                    count: p.count,
                    goalruns: p.goalruns,
                    goalsucc: p.goalsucc,
                    cmds: p.cmds,
                    stablecount: p.stablecount,
                    maxscore: p.maxscore,
                    score_tries: p.score_tries,
                    best,
                    priority: None,
                },
            );
        }
        self.set_stats(stats);

        for id in &stable {
            self.restore_stable(*id);
        }
        self.reindex();
        self.stat_ids().len()
    }

    /// Fold another search's knowledge into this one.
    ///
    /// Parallel workers start from the same snapshot and then diverge, so the
    /// merge is idempotent rather than additive: counts and successes take the
    /// best either side has seen, `first` takes the earliest, stable tokens and
    /// scores take the union. That keeps repeated syncs from inflating the
    /// numbers the goal priority is computed from.
    pub fn merge(&mut self, checkpoint: Checkpoint) {
        let mut stats: HashMap<TokenId, Stat> = self
            .stat_ids()
            .into_iter()
            .map(|id| (id, self.stat(id).clone()))
            .collect();

        for p in checkpoint.stats {
            let id = self.tokens.intern(&p.token).0;
            let incoming_tries = p.score_tries;
            let best = p.best.map(|turns| {
                turns
                    .into_iter()
                    .map(|t| Turn {
                        cmd: t.cmd,
                        replay: t.replay,
                        toks: t.toks.iter().map(|s| self.tokens.intern(s).0).collect(),
                    })
                    .collect::<Vec<Turn>>()
            });

            match stats.get_mut(&id) {
                None => {
                    stats.insert(
                        id,
                        Stat {
                            first: if p.first > FIRST_UNKNOWN {
                                FIRST_UNKNOWN
                            } else {
                                p.first
                            },
                            count: p.count,
                            goalruns: p.goalruns,
                            goalsucc: p.goalsucc,
                            cmds: p.cmds,
                            stablecount: p.stablecount,
                            maxscore: p.maxscore,
                            score_tries: incoming_tries,
                            best,
                            priority: None,
                        },
                    );
                }
                Some(stat) => {
                    stat.count = stat.count.max(p.count);
                    stat.goalruns = stat.goalruns.max(p.goalruns);
                    stat.goalsucc = stat.goalsucc.max(p.goalsucc);
                    stat.stablecount = stat.stablecount.max(p.stablecount);
                    // Keep `score_tries` paired with the winning score, and
                    // prefer the freshest attempt count when both sides tie.
                    if p.maxscore > stat.maxscore {
                        stat.maxscore = p.maxscore;
                        stat.score_tries = incoming_tries;
                    } else if p.maxscore == stat.maxscore && incoming_tries < stat.score_tries {
                        stat.score_tries = incoming_tries;
                    }
                    stat.first = stat.first.min(if p.first > FIRST_UNKNOWN {
                        FIRST_UNKNOWN
                    } else {
                        p.first
                    });
                    // The path in `best` is "shortest known"; keep whichever
                    // is shorter, preferring the one already held on a tie.
                    let keep_current = match (&stat.best, &best) {
                        (Some(current), Some(incoming)) => current.len() <= incoming.len(),
                        (Some(_), None) => true,
                        (None, Some(_)) => false,
                        (None, None) => true,
                    };
                    if !keep_current {
                        stat.best = best;
                    }
                    for cmd in p.cmds {
                        if !stat.cmds.contains(&cmd) {
                            stat.cmds.push(cmd);
                        }
                    }
                }
            }
        }

        self.set_stats(stats);
        for name in checkpoint.stabletoks {
            let id = self.tokens.intern(&name).0;
            self.restore_stable(id);
        }
        self.reindex();
    }

    /// `(count, goalruns, goalsucc)` per token, keyed by name. Workers use it
    /// as a baseline so each batch can report only the counters it added.
    pub fn counter_snapshot(&self) -> HashMap<String, (u64, u64, u64)> {
        self.stat_ids()
            .into_iter()
            .map(|id| {
                let stat = self.stat(id);
                (
                    self.tokens.name(id).to_string(),
                    (stat.count, stat.goalruns, stat.goalsucc),
                )
            })
            .collect()
    }

    /// Fold a worker batch whose `count`, `goalruns` and `goalsucc` are
    /// *increments since that worker's baseline*. Unlike [`Self::merge`], those
    /// three fields are added instead of maxed: every game belongs to exactly
    /// one worker, so the global totals are the sum of the deltas. Everything
    /// else (first, best path, stable set, score) is still best-of-either.
    pub fn merge_delta(&mut self, checkpoint: Checkpoint) {
        let mut stats: HashMap<TokenId, Stat> = self
            .stat_ids()
            .into_iter()
            .map(|id| (id, self.stat(id).clone()))
            .collect();

        for p in checkpoint.stats {
            let id = self.tokens.intern(&p.token).0;
            let incoming_tries = p.score_tries;
            let first = if p.first > FIRST_UNKNOWN {
                FIRST_UNKNOWN
            } else {
                p.first
            };
            let best = p.best.map(|turns| {
                turns
                    .into_iter()
                    .map(|t| Turn {
                        cmd: t.cmd,
                        replay: t.replay,
                        toks: t.toks.iter().map(|s| self.tokens.intern(s).0).collect(),
                    })
                    .collect::<Vec<Turn>>()
            });

            match stats.get_mut(&id) {
                None => {
                    stats.insert(
                        id,
                        Stat {
                            first,
                            count: p.count,
                            goalruns: p.goalruns,
                            goalsucc: p.goalsucc,
                            cmds: p.cmds,
                            stablecount: p.stablecount,
                            maxscore: p.maxscore,
                            score_tries: incoming_tries,
                            best,
                            priority: None,
                        },
                    );
                }
                Some(stat) => {
                    stat.count = stat.count.saturating_add(p.count);
                    stat.goalruns = stat.goalruns.saturating_add(p.goalruns);
                    stat.goalsucc = stat.goalsucc.saturating_add(p.goalsucc);
                    stat.stablecount = stat.stablecount.max(p.stablecount);
                    if p.maxscore > stat.maxscore {
                        stat.maxscore = p.maxscore;
                        stat.score_tries = incoming_tries;
                    } else if p.maxscore == stat.maxscore && incoming_tries < stat.score_tries {
                        stat.score_tries = incoming_tries;
                    }
                    stat.first = stat.first.min(first);
                    let keep_current = match (&stat.best, &best) {
                        (Some(current), Some(incoming)) => current.len() <= incoming.len(),
                        (Some(_), None) => true,
                        (None, Some(_)) => false,
                        (None, None) => true,
                    };
                    if !keep_current {
                        stat.best = best;
                    }
                    for cmd in p.cmds {
                        if !stat.cmds.contains(&cmd) {
                            stat.cmds.push(cmd);
                        }
                    }
                }
            }
        }

        self.set_stats(stats);
        for name in checkpoint.stabletoks {
            let id = self.tokens.intern(&name).0;
            self.restore_stable(id);
        }
        self.reindex();
    }
}

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

#[derive(Serialize, Deserialize)]
pub struct Checkpoint {
    pub numplays: u64,
    pub stabletoks: Vec<String>,
    pub stats: Vec<PersistStat>,
}

#[derive(Serialize, Deserialize)]
pub struct PersistStat {
    pub token: String,
    pub count: u64,
    pub goalruns: u64,
    pub goalsucc: u64,
    pub first: usize,
    pub cmds: Vec<String>,
    pub stablecount: i64,
    pub maxscore: i64,
    #[serde(default)]
    pub best: Option<Vec<PersistTurn>>,
}

#[derive(Serialize, Deserialize)]
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
                    first: if p.first > FIRST_UNKNOWN { FIRST_UNKNOWN } else { p.first },
                    count: p.count,
                    goalruns: p.goalruns,
                    goalsucc: p.goalsucc,
                    cmds: p.cmds,
                    stablecount: p.stablecount,
                    maxscore: p.maxscore,
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
}

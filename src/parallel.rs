//! Worker threads around one search each.
//!
//! The search state is not `Send` (token names are `Rc<str>`), so workers own
//! their search entirely and only exchange plain [`Checkpoint`] values. Workers
//! start from the master's snapshot, play a batch, and report back; the master
//! merges with idempotent "best of either" rules and can periodically hand the
//! merged snapshot back out so the workers share discoveries.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use crate::checkpoint::Checkpoint;
use crate::search::{GameEnd, Search};
use crate::tui::Tui;

pub struct Options {
    pub story: Vec<u8>,
    pub threads: usize,
    pub seed: u64,
    pub turns: isize,
    pub events: bool,
    pub verbose: bool,
    pub runs: u64,
    pub batch: u64,
    /// Re-share merged knowledge every N games (0 = only at the end).
    pub sync: u64,
}

enum Job {
    Run {
        checkpoint: Option<Checkpoint>,
        games: u64,
    },
    Stop,
}

struct Batch {
    worker: usize,
    games: u64,
    checkpoint: Checkpoint,
    events: Vec<String>,
    last_turns: isize,
    last_end: GameEnd,
    vocab: usize,
}

/// Play games on `threads` workers, folding their findings into `master`.
/// Returns the merged search (also used for the checkpoint and final report).
pub fn run(mut master: Search, opts: Options, ui: Tui) -> Search {
    let threads = opts.threads.max(1);
    let batch = opts.batch.max(1);
    let target = if opts.runs == 0 { u64::MAX } else { opts.runs };
    let base_plays = master.numplays;
    let checkpoint_path = master.checkpoint_path.clone();

    let (result_tx, result_rx) = mpsc::channel::<Batch>();
    let mut job_txs: Vec<Sender<Job>> = Vec::new();
    let mut handles = Vec::new();

    for id in 0..threads {
        let (job_tx, job_rx) = mpsc::channel::<Job>();
        job_txs.push(job_tx);

        let tx = result_tx.clone();
        let ui = ui.clone();
        let story = opts.story.clone();
        let seed = worker_seed(opts.seed, id);
        let turns = opts.turns;
        let events = opts.events;
        let verbose = opts.verbose;

        let handle = thread::Builder::new()
            .name(format!("zmfuzz-{}", id))
            .spawn(move || worker(id, story, seed, turns, events, verbose, job_rx, tx, ui))
            .expect("spawn worker thread");
        handles.push(handle);
    }
    // Only workers hold result senders now, so a full panic shows up as a
    // disconnected channel instead of blocking forever.
    drop(result_tx);
    ui.set_workers(threads);

    let mut assigned = 0u64;
    let mut completed = 0u64;
    let mut last_save = 0u64;
    let mut next_sync = if opts.sync == 0 { u64::MAX } else { opts.sync };
    let mut sync_snapshot = Some(master.to_checkpoint());

    for (index, job_tx) in job_txs.iter().enumerate() {
        // Spread a small run over all the threads instead of filling the first
        // worker with the whole (smaller than a batch) target.
        let remaining = target - assigned;
        let workers_left = (threads - index) as u64;
        let share = remaining.saturating_add(workers_left - 1) / workers_left;
        let give = remaining.min(batch).min(share);
        if give == 0 {
            let _ = job_tx.send(Job::Stop);
        } else {
            assigned += give;
            let _ = job_tx.send(Job::Run {
                checkpoint: sync_snapshot.clone(),
                games: give,
            });
        }
    }

    loop {
        if completed >= target || ui.quit_requested() {
            break;
        }
        ui.render(&master);

        match result_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(batch_result) => {
                let worker = batch_result.worker;
                let vocab = master.vocab_size().max(batch_result.vocab);
                completed += batch_result.games;
                master.merge(batch_result.checkpoint);
                master.numplays = base_plays + completed;

                if !ui.enabled() {
                    for event in &batch_result.events {
                        println!("{}", event);
                    }
                    println!(
                        "run {:3} w{}: plays={} turns={:3} tokens={:5} vocab={:4} stable={:3} {}",
                        completed,
                        worker,
                        master.numplays,
                        batch_result.last_turns,
                        master.token_count(),
                        vocab,
                        master.stable_count(),
                        batch_result.last_end.label()
                    );
                }

                // Auto-save from the coordinator: worker searches have no
                // checkpoint path, so only the merged state is ever written.
                if let Some(path) = &checkpoint_path {
                    if completed / 10_000 > last_save / 10_000 {
                        last_save = completed;
                        if let Err(e) = master.save_checkpoint(path) {
                            eprintln!("zmfuzz: cannot write {}: {}", path.display(), e);
                        }
                    }
                }

                let give = if completed < target && !ui.quit_requested() {
                    (target - assigned).min(batch)
                } else {
                    0
                };
                if give == 0 {
                    let _ = job_txs[worker].send(Job::Stop);
                } else {
                    if opts.sync > 0 && completed >= next_sync {
                        sync_snapshot = Some(master.to_checkpoint());
                        next_sync = completed.saturating_add(opts.sync);
                    }
                    assigned += give;
                    let _ = job_txs[worker].send(Job::Run {
                        checkpoint: sync_snapshot.clone(),
                        games: give,
                    });
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                eprintln!("zmfuzz: all workers stopped");
                break;
            }
        }

        ui.render(&master);
    }

    // Ask everyone to stop, then collect anything already in flight.
    for job_tx in &job_txs {
        let _ = job_tx.send(Job::Stop);
    }
    drop(job_txs);

    for handle in handles {
        if handle.join().is_err() {
            eprintln!("zmfuzz: a worker thread panicked");
        }
    }

    for batch_result in result_rx.try_iter() {
        completed += batch_result.games;
        master.merge(batch_result.checkpoint);
    }
    master.numplays = base_plays + completed;
    ui.render(&master);

    master
}

#[allow(clippy::too_many_arguments)]
fn worker(
    id: usize,
    story: Vec<u8>,
    seed: u64,
    turns: isize,
    events: bool,
    verbose: bool,
    jobs: Receiver<Job>,
    results: Sender<Batch>,
    ui: Tui,
) {
    let mut search = Search::new(story, seed, turns, false);
    search.usetech = events;
    search.checkpoint_path = None;
    search.progress = ui.progress_callback(id);

    while let Ok(job) = jobs.recv() {
        let (checkpoint, games) = match job {
            Job::Stop => break,
            Job::Run { checkpoint, games } => (checkpoint, games),
        };

        if let Some(checkpoint) = checkpoint {
            search.load(checkpoint);
        }

        let mut batch_events = Vec::new();
        let mut last_turns = 0;
        let mut last_end = GameEnd::Turns;

        for _ in 0..games {
            last_end = search.play_game();
            last_turns = search.last_turns();

            let events = search.take_events();
            // Worker rows are tracked even headless, so a `--screen` snapshot
            // at the end of a parallel run is not blank.
            ui.game_done(id, &search, last_end);
            if ui.enabled() {
                ui.push_events(events);
            } else if verbose {
                batch_events.extend(events);
            }
        }

        let checkpoint = search.to_checkpoint();
        let vocab = search.vocab_size();
        if results
            .send(Batch {
                worker: id,
                games,
                checkpoint,
                events: batch_events,
                last_turns,
                last_end,
                vocab,
            })
            .is_err()
        {
            break;
        }
    }
}

/// Split a user seed into per-worker seeds. Worker 0 keeps the original seed so
/// `--threads 1` stays byte-for-byte compatible with an unthreaded run.
fn worker_seed(seed: u64, id: usize) -> u64 {
    if id == 0 {
        return seed;
    }
    let mut x = seed ^ (id as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

#[cfg(test)]
mod tests {
    use super::worker_seed;

    #[test]
    fn worker_zero_keeps_the_seed() {
        assert_eq!(worker_seed(1234, 0), 1234);
    }

    #[test]
    fn worker_seeds_differ() {
        let seeds: Vec<u64> = (0..8).map(|id| worker_seed(1, id)).collect();
        for i in 0..seeds.len() {
            for j in (i + 1)..seeds.len() {
                assert_ne!(seeds[i], seeds[j]);
            }
        }
    }
}

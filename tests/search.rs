//! End-to-end checks on a real story file: determinism, checkpointing, and the
//! "this game never asks for input" case.

use std::path::PathBuf;

use zmfuzz::search::{GameEnd, Search};

fn story(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../folly/encrusted-heart/tests")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("cannot read {}: {}", path.display(), e))
}

fn named_queue(search: &Search) -> Vec<(f64, String)> {
    search
        .goal_queue()
        .into_iter()
        .map(|(priority, id)| (priority, search.token_name(id).to_string()))
        .collect()
}

fn token_names(search: &Search) -> Vec<String> {
    let mut names: Vec<String> = search
        .stat_ids()
        .into_iter()
        .map(|id| search.token_name(id).to_string())
        .collect();
    names.sort();
    names
}

#[test]
fn same_seed_is_reproducible() {
    let run = || {
        let mut search = Search::new(story("minizork.z3"), 42, 40, false);
        for _ in 0..3 {
            search.play_game();
        }
        (
            token_names(&search),
            named_queue(&search),
            search.stable_count(),
        )
    };

    let (a, qa, sa) = run();
    let (b, qb, sb) = run();
    assert_eq!(a, b);
    assert_eq!(sa, sb);
    assert_eq!(qa.len(), qb.len());
    for ((pa, ta), (pb, tb)) in qa.iter().zip(qb.iter()) {
        assert_eq!(ta, tb);
        assert!((pa - pb).abs() < 1e-9, "{} != {}", pa, pb);
    }
    assert!(a.len() > 10, "expected some tokens, got {}", a.len());
}

#[test]
fn checkpoint_round_trip_preserves_knowledge() {
    let dir = std::env::temp_dir().join("zmfuzz-test-checkpoint");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("minizork.save");

    let mut first = Search::new(story("minizork.z3"), 7, 40, false);
    for _ in 0..5 {
        first.play_game();
    }
    first.save_checkpoint(&path).unwrap();
    let names = token_names(&first);
    let queue = named_queue(&first);

    let mut second = Search::new(story("minizork.z3"), 7, 40, false);
    second.load_checkpoint(&path).unwrap();

    assert_eq!(second.numplays, first.numplays);
    assert_eq!(token_names(&second), names);
    assert_eq!(second.stable_count(), first.stable_count());
    // Priorities are recomputed on load and must match the originals.
    let reloaded = named_queue(&second);
    assert_eq!(reloaded.len(), queue.len());
    for ((pa, ta), (pb, tb)) in queue.iter().zip(reloaded.iter()) {
        assert_eq!(ta, tb, "queue order changed across checkpoint");
        assert!((pa - pb).abs() < 1e-9, "{} != {}", pa, pb);
    }

    std::fs::remove_file(&path).ok();
}

#[test]
fn resume_shifts_the_stream_past_recorded_plays() {
    let base_seed = 3;
    let mut base = Search::new(story("minizork.z3"), base_seed, 40, false);
    base.play_game();
    let checkpoint = base.to_checkpoint();

    // Without a reseed the continuation replays the sequence the checkpoint
    // already covered; that is the behaviour resuming has to avoid.
    let mut straight = Search::new(story("minizork.z3"), base_seed, 40, false);
    straight.load(checkpoint.clone());
    straight.play_game();

    let shifted = base_seed.wrapping_add(checkpoint.numplays);
    let mut resumed = Search::new(story("minizork.z3"), base_seed, 40, false);
    resumed.load(checkpoint.clone());
    resumed.reseed(shifted);
    resumed.play_game();

    // Reseeding is equivalent to having started at the shifted seed with the
    // same accumulated knowledge.
    let mut fresh = Search::new(story("minizork.z3"), shifted, 40, false);
    fresh.load(checkpoint);
    fresh.play_game();
    assert_eq!(token_names(&resumed), token_names(&fresh));

    assert_ne!(
        token_names(&straight),
        token_names(&resumed),
        "resumed run replayed the checkpoint's games"
    );
}

#[test]
fn conformance_battery_never_asks_for_input() {
    // czech.z5 runs its tests and ends with "quit!", so there is nothing to fuzz.
    let mut search = Search::new(story("czech.z5"), 1, 50, false);
    assert_eq!(search.play_game(), GameEnd::Quit);
    assert_eq!(search.last_turns(), 0);
    assert_eq!(search.token_count(), 0);
}

#[test]
fn merge_unions_worker_knowledge_and_is_idempotent() {
    let mut first = Search::new(story("minizork.z3"), 11, 40, false);
    first.play_game();

    let mut second = Search::new(story("minizork.z3"), 12, 40, false);
    for _ in 0..2 {
        second.play_game();
    }
    let second_names = token_names(&second);
    let checkpoint = second.to_checkpoint();

    first.merge(checkpoint.clone());
    let merged_names = token_names(&first);
    for name in &second_names {
        assert!(merged_names.contains(name), "merge lost token {}", name);
    }
    let after_first = (
        first.token_count(),
        first.stable_count(),
        named_queue(&first),
    );

    // Applying the same worker snapshot again must not inflate the counters
    // the goal priority is derived from.
    first.merge(checkpoint);
    let after_second = (
        first.token_count(),
        first.stable_count(),
        named_queue(&first),
    );
    assert_eq!(after_first.0, after_second.0);
    assert_eq!(after_first.1, after_second.1);
    assert_eq!(after_first.2.len(), after_second.2.len());
    for ((pa, ta), (pb, tb)) in after_first.2.iter().zip(after_second.2.iter()) {
        assert_eq!(ta, tb);
        assert!((pa - pb).abs() < 1e-9, "{} != {}", pa, pb);
    }
}

#[test]
fn fuzzes_a_v3_game() {
    let mut search = Search::new(story("minizork.z3"), 3, 60, false);
    assert_eq!(search.play_game(), GameEnd::Turns);
    assert!(search.last_turns() >= 60);
    assert!(search.token_count() > 5, "no tokens recorded");
    assert!(search.vocab_size() > 100, "dictionary not loaded");
}

#[test]
fn parallel_workers_merge_into_the_master() {
    use zmfuzz::parallel;
    use zmfuzz::tui::{Screen, Tui, TuiConfig};

    let ui = Tui::new(
        false,
        TuiConfig {
            title: "minizork.z3".to_string(),
            threads: 2,
            turns: 30,
            seed: 21,
            target: 4,
            checkpoint: String::new(),
            screen: Screen::Now,
        },
    );
    let master = Search::new(story("minizork.z3"), 21, 30, false);
    let merged = parallel::run(
        master,
        parallel::Options {
            story: story("minizork.z3"),
            threads: 2,
            seed: 21,
            turns: 30,
            events: true,
            verbose: false,
            runs: 4,
            batch: 1,
            sync: 2,
        },
        ui,
    );

    assert_eq!(merged.numplays, 4);
    assert!(
        merged.token_count() > 5,
        "parallel run recorded no tokens: {}",
        merged.token_count()
    );
    assert!(merged.vocab_size() == 0 || merged.vocab_size() > 100);
}

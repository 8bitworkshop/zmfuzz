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
fn conformance_battery_never_asks_for_input() {
    // czech.z5 runs its tests and ends with "quit!", so there is nothing to fuzz.
    let mut search = Search::new(story("czech.z5"), 1, 50, false);
    assert_eq!(search.play_game(), GameEnd::Quit);
    assert_eq!(search.last_turns(), 0);
    assert_eq!(search.token_count(), 0);
}

#[test]
fn fuzzes_a_v3_game() {
    let mut search = Search::new(story("minizork.z3"), 3, 60, false);
    assert_eq!(search.play_game(), GameEnd::Turns);
    assert!(search.last_turns() >= 60);
    assert!(search.token_count() > 5, "no tokens recorded");
    assert!(search.vocab_size() > 100, "dictionary not loaded");
}

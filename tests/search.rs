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

fn named_priority(search: &Search, name: &str) -> f64 {
    let id = search.tokens.get(name).expect("missing token");
    search
        .stat(id)
        .priority
        .as_ref()
        .expect("token has no priority")
        .value
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

#[test]
fn state_tokens_display_with_object_names() {
    let mut search = Search::new(story("minizork.z3"), 7, 20, false);
    search.play_game();

    // Object 30 is the player and 46 is a room in Mini-Zork; the ids stay put,
    // only the display changes.
    assert_eq!(search.label("@mv_30_46"), "you → West of House");
    assert_eq!(search.label("@f_30_8_1"), "you flag8=1");
    // Six rooms share the name "Maze", so the label has to keep them apart.
    assert_eq!(search.label("@mv_30_134"), "you → Maze#134");
    assert_eq!(search.label("@mv_30_57"), "you → Maze#57");
    // Printed chunks are not tokens and pass through untouched.
    assert_eq!(search.label("You are in a maze"), "You are in a maze");
}

#[test]
fn the_hack_event_names_the_object_it_moves() {
    use std::collections::HashMap;
    use zmfuzz::stat::Stat;

    let mut search = Search::new(story("minizork.z3"), 3, 20, false);
    search.play_game();

    // Make a move token look long-settled so `hackvm` will teleport it: it only
    // fires on a stable `@mv_` goal at turn 0.
    let id = search.tokens.intern("@mv_30_46").0;
    let mut stat = Stat::new("north".to_string());
    stat.stablecount = 20;
    search.set_stats(HashMap::from([(id, stat)]));
    search.restore_stable(id);
    search.reindex();

    let mut hacks = Vec::new();
    for _ in 0..200 {
        search.play_game();
        hacks.extend(
            search
                .take_events()
                .into_iter()
                .filter(|event| event.starts_with("HACK ")),
        );
        if !hacks.is_empty() {
            break;
        }
    }

    assert!(!hacks.is_empty(), "no HACK in 200 games");
    assert!(
        hacks.iter().all(|event| event.contains("you → ")),
        "HACK line keeps raw ids: {:?}",
        hacks
    );
}

#[test]
fn reading_names_does_not_disturb_the_search() {
    let play = |teach: bool| {
        let mut search = Search::new(story("minizork.z3"), 99, 25, false);
        if teach {
            // The parallel coordinator does this before printing its summary.
            search.ensure_labels();
            assert_eq!(search.label("@mv_30_46"), "you → West of House");
        }
        for _ in 0..3 {
            search.play_game();
        }
        token_names(&search)
    };

    assert_eq!(play(true), play(false), "label lookup drew from the RNG");
}

#[test]
fn score_bonus_decays_and_failures_push_goals_down() {
    use std::collections::HashMap;
    use zmfuzz::stat::Stat;

    let make = |first, goalruns, goalsucc, maxscore, score_tries| {
        let mut stat = Stat::new("x".to_string());
        stat.first = first;
        stat.count = goalruns * 5;
        stat.goalruns = goalruns;
        stat.goalsucc = goalsucc;
        stat.maxscore = maxscore;
        stat.score_tries = score_tries;
        stat
    };

    let mut search = Search::new(story("minizork.z3"), 1, 20, false);
    let fresh = search.tokens.intern("score-fresh").0;
    let aged = search.tokens.intern("score-aged").0;
    let cold = search.tokens.intern("score-cold").0;
    let plain = search.tokens.intern("score-plain").0;
    search.set_stats(HashMap::from([
        (fresh, make(4, 5, 2, 6, 0)),
        // Same score and history, but many attempts without improving it.
        (aged, make(4, 5, 2, 6, 200)),
        // A goal that has never succeeded: the failure penalty should bury it.
        (cold, make(5, 20, 0, 0, 0)),
        (plain, make(5, 0, 0, 0, 0)),
    ]));
    search.reindex();

    let pf = named_priority(&search, "score-plain");
    let p0 = named_priority(&search, "score-fresh");
    let p200 = named_priority(&search, "score-aged");
    let pc = named_priority(&search, "score-cold");

    assert!(p0 < pf, "fresh score bonus should still be attractive");
    assert!(
        p200 > pf,
        "score bonus did not decay enough: {} vs {}",
        p200,
        pf
    );
    assert!(pc > pf, "never-succeeding goal was not penalized");
}

#[test]
fn parallel_counters_are_additive_across_workers() {
    use zmfuzz::parallel;
    use zmfuzz::tui::{Screen, Tui, TuiConfig};

    let ui = Tui::new(
        false,
        TuiConfig {
            title: "minizork.z3".to_string(),
            threads: 2,
            turns: 30,
            seed: 31,
            target: 8,
            checkpoint: String::new(),
            screen: Screen::Now,
        },
    );
    // Seed the master so every worker game starts with a non-empty goal queue.
    let mut master = Search::new(story("minizork.z3"), 31, 30, false);
    master.play_game();
    let base_plays = master.numplays;

    let merged = parallel::run(
        master,
        parallel::Options {
            story: story("minizork.z3"),
            threads: 2,
            seed: 31,
            turns: 30,
            events: true,
            verbose: false,
            runs: 8,
            batch: 1,
            sync: 4,
        },
        ui,
    );

    assert_eq!(merged.numplays, base_plays + 8);
    let goalruns: u64 = merged
        .stat_ids()
        .into_iter()
        .map(|id| merged.stat(id).goalruns)
        .sum();
    assert_eq!(goalruns, 8, "parallel merge lost goal attempts");
}

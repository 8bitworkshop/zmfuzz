//! Command-line driver: parse arguments, play games, print progress.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process;

use zmfuzz::parallel;
use zmfuzz::search::Search;
use zmfuzz::tui::{Screen, Tui, TuiConfig};

struct Args {
    story: PathBuf,
    runs: u64,
    turns: isize,
    seed: u64,
    top: usize,
    threads: usize,
    batch: u64,
    sync: u64,
    log: bool,
    verbose: bool,
    events: bool,
    ui: Option<bool>,
    checkpoint: Option<PathBuf>,
    resume: bool,
    fresh: bool,
    replay: Option<PathBuf>,
    screen: Option<String>,
}

fn usage() -> ! {
    eprintln!(
        "\
zmfuzz -- fuzz a Z-machine story, learning the shortest path to every output

USAGE:
    zmfuzz <story.z3> [options]

OPTIONS:
    --runs N       number of games to play (0 = forever, default 1)
    --turns N      turns per game (default 200)
    --seed N       RNG seed (default 1; same seed = same search). A resumed
                 run starts at base+plays, so each invocation gets new ground
    --top N        how many frequent tokens to list (default 20)
    --threads N    worker threads (default 1)
    --batch N      games per worker before reporting (default 10)
    --sync N       re-share merged knowledge every N games (0 = at end only)
    --checkpoint F read/write search state at F (default ./<story>.save)
    --resume       force loading the checkpoint (the default when it exists)
    --fresh        ignore an existing checkpoint and start blank
    --replay F     feed commands from F, print the tokens each turn produced
    --no-events    ignore VM side effects (text tokens only)
    --log          trace every VM instruction on stderr (very noisy)
    --ui           live full-screen display (default: on when stdout is a TTY)
    --no-ui        plain per-run output even on a terminal
    --screen S     start on pane S: now tree map tokens workers log
                   (default now; 1-6/Tab switch panes while running)
    -v, --verbose  print NEWTOKEN/REDUCE/STABLE/goal events
    -h, --help     this text"
    );
    process::exit(2);
}

fn parse_args() -> Args {
    let mut args = Args {
        story: PathBuf::new(),
        runs: 1,
        turns: 200,
        seed: 1,
        top: 20,
        threads: 1,
        batch: 10,
        sync: 1000,
        log: false,
        verbose: false,
        events: true,
        ui: None,
        checkpoint: None,
        resume: false,
        fresh: false,
        replay: None,
        screen: None,
    };
    let mut story: Option<PathBuf> = None;

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--runs" => args.runs = next_value(&mut it, "--runs").parse().unwrap_or(1),
            "--turns" => args.turns = next_value(&mut it, "--turns").parse().unwrap_or(200),
            "--seed" => args.seed = next_value(&mut it, "--seed").parse().unwrap_or(1),
            "--top" => args.top = next_value(&mut it, "--top").parse().unwrap_or(20),
            "--threads" => {
                args.threads = next_value(&mut it, "--threads")
                    .parse::<usize>()
                    .unwrap_or(1)
                    .max(1)
            }
            "--batch" => args.batch = next_value(&mut it, "--batch").parse().unwrap_or(10).max(1),
            "--sync" => args.sync = next_value(&mut it, "--sync").parse().unwrap_or(1000),
            "--checkpoint" => {
                args.checkpoint = Some(PathBuf::from(next_value(&mut it, "--checkpoint")))
            }
            "--resume" => args.resume = true,
            "--fresh" | "--no-resume" => args.fresh = true,
            "--replay" => args.replay = Some(PathBuf::from(next_value(&mut it, "--replay"))),
            "--screen" => args.screen = Some(next_value(&mut it, "--screen")),
            "--log" => args.log = true,
            "--no-events" => args.events = false,
            "--ui" => args.ui = Some(true),
            "--no-ui" => args.ui = Some(false),
            "-v" | "--verbose" => args.verbose = true,
            "-h" | "--help" => usage(),
            other if other.starts_with('-') => {
                eprintln!("zmfuzz: unknown option {}", other);
                usage();
            }
            other => story = Some(PathBuf::from(other)),
        }
    }

    args.story = story.unwrap_or_else(|| usage());
    if args.checkpoint.is_none() {
        // Beside the *working directory*, not the story: fuzzing should not
        // scribble next to the test files.
        let name = args
            .story
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "zmfuzz".to_string());
        args.checkpoint = Some(PathBuf::from(format!("{}.save", name)));
    }
    args
}

fn next_value(it: &mut impl Iterator<Item = String>, flag: &str) -> String {
    it.next().unwrap_or_else(|| {
        eprintln!("zmfuzz: {} needs a value", flag);
        usage();
    })
}

fn main() {
    let args = parse_args();

    let story = match std::fs::read(&args.story) {
        Ok(story) => story,
        Err(e) => {
            eprintln!("zmfuzz: cannot read {}: {}", args.story.display(), e);
            process::exit(1);
        }
    };
    if story.len() < 64 {
        eprintln!(
            "zmfuzz: {} is too small to be a story file",
            args.story.display()
        );
        process::exit(1);
    }

    // A full-screen display makes no sense for replay, and instruction tracing
    // is stderr chatter that would scribble over it.
    let ui_enabled = args
        .ui
        .unwrap_or_else(|| args.replay.is_none() && !args.log && std::io::stdout().is_terminal());

    let screen = match &args.screen {
        Some(name) => match Screen::parse(name) {
            Some(screen) => screen,
            None => {
                eprintln!("zmfuzz: unknown screen {}", name);
                usage();
            }
        },
        None => Screen::Now,
    };

    let worker_story = story.clone();
    let mut search = Search::new(story, args.seed, args.turns, args.verbose && !ui_enabled);
    search.log_instructions = args.log;
    search.usetech = args.events;
    search.checkpoint_path = args.checkpoint.clone();

    if let Some(path) = &args.replay {
        let body = std::fs::read_to_string(path).unwrap_or_else(|e| {
            eprintln!("zmfuzz: cannot read {}: {}", path.display(), e);
            process::exit(1);
        });
        let commands: Vec<String> = body
            .lines()
            .map(|line| line.trim_end_matches('\n'))
            .filter(|line| !line.is_empty())
            .map(|line| line.to_string())
            .collect();
        for (index, cmd, toks) in search.replay_commands(&commands) {
            println!("TURN {} {} | {}", index, cmd, toks.join(" "));
        }
        return;
    }

    // Resuming is the default: a checkpoint on disk is accumulated knowledge,
    // and the run would overwrite it on exit anyway. `--fresh` opts out, and an
    // explicit `--resume` still reports a missing file as an error.
    let mut seed = args.seed;
    if let Some(path) = &args.checkpoint {
        let exists = path.exists();
        if args.resume || (exists && !args.fresh) {
            match search.load_checkpoint(path) {
                Ok(()) => {
                    // Advance the stream by the work already done, so a resumed
                    // run explores new games instead of replaying the old ones.
                    seed = args.seed.wrapping_add(search.numplays);
                    search.reseed(seed);
                    println!(
                        "resumed from {}: {} plays, {} tokens, {} stable (seed {} -> {})",
                        path.display(),
                        search.numplays,
                        search.token_count(),
                        search.stable_count(),
                        args.seed,
                        seed
                    );
                }
                Err(e) => eprintln!("zmfuzz: cannot resume from {}: {}", path.display(), e),
            }
        } else if exists && args.fresh {
            println!("starting fresh; {} will be overwritten", path.display());
        }
    }

    println!(
        "zmfuzz: v{} story, {} games of {} turns, seed {}, {} thread{}",
        search.version(),
        if args.runs == 0 {
            "unbounded".to_string()
        } else {
            args.runs.to_string()
        },
        args.turns,
        seed,
        args.threads,
        if args.threads == 1 { "" } else { "s" }
    );

    let title = args
        .story
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| args.story.display().to_string());
    let mut ui = Tui::new(
        ui_enabled,
        TuiConfig {
            title,
            threads: args.threads,
            turns: args.turns,
            seed,
            target: args.runs,
            checkpoint: args
                .checkpoint
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            screen,
        },
    );

    search = if args.threads <= 1 {
        run_sequential(search, args.runs, &ui)
    } else {
        let verbose = search.verbose;
        parallel::run(
            search,
            parallel::Options {
                story: worker_story,
                threads: args.threads,
                seed,
                turns: args.turns,
                events: args.events,
                verbose,
                runs: args.runs,
                batch: args.batch,
                sync: args.sync,
            },
            ui.clone(),
        )
    };

    // The display owns the alternate screen; hand the terminal back before the
    // final, scrollback-friendly report.
    ui.finish();

    // A parallel run leaves the coordinator without a single played game, so it
    // has not read the object table yet; the summary would print raw ids.
    search.ensure_labels();

    // Without a terminal, `--screen X` still gets you the pane: one static
    // frame, wide enough for the tab strip, printed once at the end.
    if !ui_enabled {
        if args.screen.is_some() {
            for line in ui.frame_text(&search, 100, 40) {
                println!("{}", line.trim_end());
            }
        }
    }

    // What the search would aim at next, cheapest first.
    let queue = search.goal_queue();
    if !queue.is_empty() {
        println!("\ngoals (cheapest first):");
        for (priority, id) in queue.iter().take(args.top) {
            let (first, runs, succ, stable, score) = search.goal_detail(*id);
            println!(
                "  {:10.3}  {:35}  first={} runs={} succ={} stable={} score={}",
                priority,
                search.token_label(*id),
                first,
                runs,
                succ,
                stable,
                score
            );
        }
    }

    if let Some(path) = &args.checkpoint {
        match search.save_checkpoint(path) {
            Ok(()) => println!("\ncheckpoint: {}", path.display()),
            Err(e) => eprintln!("\nzmfuzz: cannot write {}: {}", path.display(), e),
        }
    }
}

fn run_sequential(mut search: Search, runs: u64, ui: &Tui) -> Search {
    ui.set_workers(1);
    // The live variant redraws (and polls keys) every turn; the plain callback
    // would leave the display frozen for the whole playthrough.
    search.progress = ui.live_callback(0);

    let mut done = 0u64;
    loop {
        if ui.quit_requested() {
            break;
        }

        let end = search.play_game();
        done += 1;

        let events = search.take_events();
        // Kept up to date even without a terminal: the panes are how a
        // `--screen X --no-ui` run reports what it learned.
        ui.push_events(events);
        ui.game_done(0, &search, end);
        if ui.enabled() {
            ui.render(&search);
        } else {
            println!(
                "run {:3}: plays={} turns={:3} tokens={:5} vocab={:4} stable={:3} {}",
                done,
                search.numplays,
                search.last_turns(),
                search.token_count(),
                search.vocab_size(),
                search.stable_count(),
                end.label()
            );
        }

        if runs != 0 && done >= runs {
            break;
        }
    }

    search
}

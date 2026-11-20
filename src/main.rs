//! Command-line driver: parse arguments, play games, print progress.

use std::path::PathBuf;
use std::process;

use zmfuzz::search::{GameEnd, Search};

struct Args {
    story: PathBuf,
    runs: u64,
    turns: isize,
    seed: u64,
    top: usize,
    log: bool,
    verbose: bool,
    events: bool,
    checkpoint: Option<PathBuf>,
    resume: bool,
    replay: Option<PathBuf>,
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
    --seed N       RNG seed (default 1; same seed = same search)
    --top N        how many frequent tokens to list (default 20)
    --checkpoint F read/write search state at F (default ./<story>.save)
    --resume       start from the checkpoint instead of a blank search
    --replay F     feed commands from F, print the tokens each turn produced
    --no-events    ignore VM side effects (text tokens only)
    --log          trace every VM instruction on stderr (very noisy)
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
        log: false,
        verbose: false,
        events: true,
        checkpoint: None,
        resume: false,
        replay: None,
    };
    let mut story: Option<PathBuf> = None;

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--runs" => args.runs = next_value(&mut it, "--runs").parse().unwrap_or(1),
            "--turns" => args.turns = next_value(&mut it, "--turns").parse().unwrap_or(200),
            "--seed" => args.seed = next_value(&mut it, "--seed").parse().unwrap_or(1),
            "--top" => args.top = next_value(&mut it, "--top").parse().unwrap_or(20),
            "--checkpoint" => args.checkpoint = Some(PathBuf::from(next_value(&mut it, "--checkpoint"))),
            "--resume" => args.resume = true,
            "--replay" => args.replay = Some(PathBuf::from(next_value(&mut it, "--replay"))),
            "--log" => args.log = true,
            "--no-events" => args.events = false,
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
        eprintln!("zmfuzz: {} is too small to be a story file", args.story.display());
        process::exit(1);
    }

    let mut search = Search::new(story, args.seed, args.turns, args.verbose);
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

    if args.resume {
        if let Some(path) = &args.checkpoint {
            match search.load_checkpoint(path) {
                Ok(()) => println!(
                    "resumed from {}: {} plays, {} tokens, {} stable",
                    path.display(),
                    search.numplays,
                    search.token_count(),
                    search.stable_count()
                ),
                Err(e) => eprintln!("zmfuzz: cannot resume from {}: {}", path.display(), e),
            }
        }
    }

    println!(
        "zmfuzz: v{} story, {} games of {} turns, seed {}",
        search.version(),
        if args.runs == 0 {
            "unbounded".to_string()
        } else {
            args.runs.to_string()
        },
        args.turns,
        args.seed
    );

    let mut runs = 0u64;
    loop {
        let end = search.play_game();
        runs += 1;

        let name = match end {
            GameEnd::Quit => "ended",
            GameEnd::Turns => "turn-limit",
            GameEnd::Stuck => "stuck",
        };
        println!(
            "run {:3}: plays={} turns={:3} tokens={:5} vocab={:4} stable={:3} {}",
            runs,
            search.numplays,
            search.last_turns(),
            search.token_count(),
            search.vocab_size(),
            search.stable_count(),
            name
        );

        if args.runs != 0 && runs >= args.runs {
            break;
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
                search.token_name(*id),
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

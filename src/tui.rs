//! A small full-screen live display: alternate screen, raw input, redraw on a
//! timer.
//!
//! Deliberately not a widget library. The layout is a handful of lines built
//! from snapshots of the worker statuses and the master search, then written
//! with crossterm. `q` or Ctrl-C asks the driver to stop; the terminal is
//! restored on drop even if the render path returns early.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, queue};

use crate::search::{GameEnd, Progress, Search};

const REDRAW: Duration = Duration::from_millis(100);
const EVENT_ROWS: usize = 256;

#[derive(Clone)]
pub struct TuiConfig {
    pub title: String,
    pub threads: usize,
    pub turns: isize,
    pub seed: u64,
    /// 0 means unbounded.
    pub target: u64,
    pub checkpoint: String,
}

#[derive(Clone, Default)]
pub struct WorkerStatus {
    pub games: u64,
    pub turn: isize,
    pub tokens: usize,
    pub vocab: usize,
    pub stable: usize,
    pub goal: String,
    pub cmd: String,
    pub seen: Vec<String>,
    pub end: String,
}

struct Inner {
    cfg: TuiConfig,
    workers: Vec<WorkerStatus>,
    events: VecDeque<String>,
    games: u64,
    quit: bool,
    last_draw: Instant,
}

struct Snapshot {
    cfg: TuiConfig,
    workers: Vec<WorkerStatus>,
    events: Vec<String>,
    games: u64,
}

type Line = (String, Color, bool);

pub struct Tui {
    inner: Arc<Mutex<Inner>>,
    enabled: bool,
    active: Arc<AtomicBool>,
    start: Instant,
    owner: bool,
}

impl Clone for Tui {
    fn clone(&self) -> Tui {
        Tui {
            inner: Arc::clone(&self.inner),
            enabled: self.enabled,
            active: Arc::clone(&self.active),
            start: self.start,
            owner: false,
        }
    }
}

impl Tui {
    pub fn new(enabled: bool, cfg: TuiConfig) -> Tui {
        let tui = Tui {
            inner: Arc::new(Mutex::new(Inner {
                cfg,
                workers: Vec::new(),
                events: VecDeque::new(),
                games: 0,
                quit: false,
                last_draw: Instant::now(),
            })),
            enabled,
            active: Arc::new(AtomicBool::new(false)),
            start: Instant::now(),
            owner: true,
        };
        if enabled {
            tui.enter();
        }
        tui
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Make one slot per worker, so the display has a stable ordering even
    /// before a worker reports anything.
    pub fn set_workers(&self, count: usize) {
        if !self.enabled {
            return;
        }
        if let Ok(mut inner) = self.inner.lock() {
            inner.workers = vec![WorkerStatus::default(); count];
        }
    }

    fn enter(&self) {
        let _ = terminal::enable_raw_mode();
        let mut out = io::stdout();
        if execute!(out, EnterAlternateScreen, Hide, Clear(ClearType::All)).is_ok() {
            self.active.store(true, Ordering::SeqCst);
        }
    }

    fn restore(&self) {
        if self.active.swap(false, Ordering::SeqCst) {
            let _ = terminal::disable_raw_mode();
            let mut out = io::stdout();
            let _ = execute!(out, Show, LeaveAlternateScreen);
            let _ = out.flush();
        }
    }

    /// Leave the alternate screen so normal reporting can resume. Idempotent.
    pub fn finish(&mut self) {
        self.restore();
    }

    pub fn quit_requested(&self) -> bool {
        self.inner.lock().map(|inner| inner.quit).unwrap_or(false)
    }

    /// A callback the search calls once per turn. One per worker.
    pub fn progress_callback(&self, id: usize) -> Option<Arc<dyn Fn(Progress) + Send + Sync>> {
        if !self.enabled {
            return None;
        }
        let inner = Arc::clone(&self.inner);
        Some(Arc::new(move |progress: Progress| {
            if let Ok(mut guard) = inner.lock() {
                if let Some(worker) = guard.workers.get_mut(id) {
                    apply_progress(worker, progress);
                }
            }
        }))
    }

    /// Fold a finished game into the worker's row.
    pub fn game_done(&self, id: usize, search: &Search, end: GameEnd) {
        if !self.enabled {
            return;
        }
        if let Ok(mut inner) = self.inner.lock() {
            inner.games += 1;
            if let Some(worker) = inner.workers.get_mut(id) {
                worker.games += 1;
                worker.turn = search.last_turns();
                worker.tokens = search.token_count();
                worker.vocab = search.vocab_size();
                worker.stable = search.stable_count();
                worker.goal = search.current_goal();
                worker.cmd = search.current_cmd();
                worker.end = end.label().to_string();
            }
        }
    }

    pub fn push_events(&self, events: Vec<String>) {
        if !self.enabled || events.is_empty() {
            return;
        }
        if let Ok(mut inner) = self.inner.lock() {
            inner.events.extend(events);
            while inner.events.len() > EVENT_ROWS {
                inner.events.pop_front();
            }
        }
    }

    /// Redraw at most every `REDRAW`; cheap to call from a hot loop.
    pub fn render(&self, master: &Search) {
        if !self.enabled {
            return;
        }
        self.poll_keys();

        let snapshot = {
            let mut inner = match self.inner.lock() {
                Ok(inner) => inner,
                Err(_) => return,
            };
            let now = Instant::now();
            if now.duration_since(inner.last_draw) < REDRAW {
                return;
            }
            inner.last_draw = now;
            Snapshot {
                cfg: inner.cfg.clone(),
                workers: inner.workers.clone(),
                events: inner.events.iter().cloned().collect(),
                games: inner.games,
            }
        };

        if let Err(e) = self.draw(master, &snapshot) {
            // A closed terminal is not worth crashing the search over.
            let _ = e;
        }
    }

    fn poll_keys(&self) {
        while event::poll(Duration::from_millis(0)).unwrap_or(false) {
            match event::read() {
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                    let ctrl_c = key.modifiers.contains(KeyModifiers::CONTROL)
                        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'));
                    if ctrl_c || matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                        if let Ok(mut inner) = self.inner.lock() {
                            inner.quit = true;
                        }
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }

    fn draw(&self, master: &Search, snapshot: &Snapshot) -> io::Result<()> {
        let (width, height) = terminal::size().unwrap_or((100, 30));
        let width = width.max(20) as usize;
        let height = height.max(6) as usize;

        let mut lines: Vec<Line> = Vec::new();
        let target = if snapshot.cfg.target == 0 {
            "∞".to_string()
        } else {
            snapshot.cfg.target.to_string()
        };
        let elapsed = self.start.elapsed().as_secs_f64();
        let rate = if elapsed > 0.0 {
            snapshot.games as f64 / elapsed
        } else {
            0.0
        };

        lines.push((
            format!(
                " zmfuzz · {} · {} turns · seed {}",
                snapshot.cfg.title, snapshot.cfg.turns, snapshot.cfg.seed
            ),
            Color::Cyan,
            true,
        ));
        lines.push((
            format!(
                " {} threads · {}/{} games · {:.1} games/s · {} · checkpoint {}",
                snapshot.cfg.threads,
                snapshot.games,
                target,
                rate,
                clock(self.start.elapsed().as_secs()),
                snapshot.cfg.checkpoint,
            ),
            Color::DarkGrey,
            false,
        ));
        lines.push((String::new(), Color::Reset, false));

        // Leave room for knowledge, goals and events below the worker rows.
        let max_workers = ((height.saturating_sub(16)) / 2).max(1);
        let shown = snapshot.workers.len().min(max_workers);
        lines.push((" WORKERS".to_string(), Color::White, true));
        for (id, worker) in snapshot.workers.iter().take(shown).enumerate() {
            lines.push((
                format!(
                    "  #{:<2} {:>7} games  {:>5} tok  {:>4} vocab  {:>4} stable  {:>4}t  {}",
                    id,
                    worker.games,
                    worker.tokens,
                    worker.vocab,
                    worker.stable,
                    worker.turn,
                    worker.end
                ),
                Color::Green,
                false,
            ));
            let goal = if worker.goal.is_empty() {
                "-"
            } else {
                worker.goal.as_str()
            };
            let seen = if worker.seen.is_empty() {
                "-".to_string()
            } else {
                worker.seen.join(" | ")
            };
            lines.push((
                format!("      goal {}  cmd \"{}\"  saw {}", goal, worker.cmd, seen),
                Color::DarkGrey,
                false,
            ));
        }
        if shown < snapshot.workers.len() {
            lines.push((
                format!("  … {} more workers", snapshot.workers.len() - shown),
                Color::DarkGrey,
                false,
            ));
        }

        lines.push((String::new(), Color::Reset, false));
        let worker_vocab = snapshot
            .workers
            .iter()
            .map(|worker| worker.vocab)
            .max()
            .unwrap_or(0);
        lines.push((
            format!(
                " KNOWLEDGE  tokens {}  vocab {}  stable {}  goal queue {}",
                master.token_count(),
                master.vocab_size().max(worker_vocab),
                master.stable_count(),
                master.goal_queue().len()
            ),
            Color::White,
            true,
        ));

        lines.push((String::new(), Color::Reset, false));
        lines.push((" GOALS (cheapest first)".to_string(), Color::White, true));
        // Reserve at least two rows for the events pane.
        let reserved = 2;
        let goal_slots = height
            .saturating_sub(lines.len())
            .saturating_sub(reserved)
            .min(8);
        let queue = master.goal_queue();
        if queue.is_empty() {
            lines.push(("  (no goals yet)".to_string(), Color::DarkGrey, false));
        } else {
            for (priority, id) in queue.iter().take(goal_slots) {
                let (first, runs, succ, stable, score) = master.goal_detail(*id);
                lines.push((
                    format!(
                        "  {:10.3}  {:30}  first={} runs={} succ={} stable={} score={}",
                        priority,
                        master.token_name(*id),
                        first,
                        runs,
                        succ,
                        stable,
                        score
                    ),
                    if stable { Color::Green } else { Color::Reset },
                    false,
                ));
            }
        }

        if lines.len() < height {
            lines.push((String::new(), Color::Reset, false));
            lines.push((" EVENTS".to_string(), Color::White, true));
        }
        let remaining = height.saturating_sub(lines.len());
        let skip = snapshot.events.len().saturating_sub(remaining);
        for event in snapshot.events.iter().skip(skip) {
            let color = event_color(event);
            lines.push((format!("  {}", event), color, false));
        }

        let mut out = io::stdout();
        queue!(out, crossterm::cursor::MoveTo(0, 0))?;
        for (index, (text, color, bold)) in lines.iter().take(height).enumerate() {
            if index > 0 {
                queue!(out, Print("\r\n"))?;
            }
            if *bold {
                queue!(out, SetAttribute(Attribute::Bold))?;
            }
            queue!(
                out,
                SetForegroundColor(*color),
                Print(fit(text, width.saturating_sub(1))),
                // Erase the tail of a previously longer line: event messages
                // shrink and grow, and \r\n alone would leave the old text.
                Clear(ClearType::UntilNewLine),
                ResetColor,
                SetAttribute(Attribute::Reset)
            )?;
        }
        queue!(out, Clear(ClearType::FromCursorDown))?;
        out.flush()
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        if self.owner {
            self.restore();
        }
    }
}

fn apply_progress(worker: &mut WorkerStatus, progress: Progress) {
    worker.turn = progress.turn;
    worker.goal = progress.goal;
    worker.cmd = progress.cmd;
    worker.seen = progress.tokens;
}

fn event_color(event: &str) -> Color {
    if event.starts_with("STABLE") || event.starts_with("UNSTABLE") {
        Color::Green
    } else if event.starts_with("HACK") {
        Color::Yellow
    } else if event.starts_with("SCORE") {
        Color::Magenta
    } else if event.starts_with("NEWTOKEN") {
        Color::Cyan
    } else if event.starts_with("REDUCE") || event.starts_with("DIVERGE") {
        Color::Blue
    } else {
        Color::DarkGrey
    }
}

/// One line, no control characters, no wider than `width` cells.
fn fit(text: &str, width: usize) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(width)
        .collect()
}

fn clock(seconds: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        (seconds / 60) % 60,
        seconds % 60
    )
}

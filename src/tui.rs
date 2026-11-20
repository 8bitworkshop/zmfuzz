//! A small full-screen live display: alternate screen, raw input, redraw on a
//! timer.
//!
//! Deliberately not a widget library. The screen is a pinned header (the answer
//! to "is it working?" never scrolls away) plus one switchable detail pane,
//! built from snapshots of the worker statuses and the master search and
//! written with crossterm. `q` or Ctrl-C asks the driver to stop; the terminal
//! is restored on drop even if the render path returns early.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, queue};

use crate::search::{GameEnd, Progress, Search};

const REDRAW: Duration = Duration::from_millis(100);
const EVENT_ROWS: usize = 512;

/// Detail panes. The header above them does not change, so switching panes
/// never hides whether the search is still learning anything.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Screen {
    Now,
    Tree,
    Map,
    Tokens,
    Workers,
    Log,
}

pub const SCREENS: usize = 6;

fn parse_screen(name: &str) -> Option<Screen> {
    let name = name.trim().to_ascii_lowercase();
    if let Ok(index) = name.parse::<usize>() {
        if (1..=SCREENS).contains(&index) {
            return Some(Screen::from_index(index - 1));
        }
    }
    match name.as_str() {
        "now" | "live" | "current" => Some(Screen::Now),
        "tree" | "trie" | "paths" => Some(Screen::Tree),
        "map" | "rooms" => Some(Screen::Map),
        "tokens" | "goals" => Some(Screen::Tokens),
        "workers" | "threads" => Some(Screen::Workers),
        "log" | "events" => Some(Screen::Log),
        _ => None,
    }
}

impl Screen {
    pub const ALL: [Screen; SCREENS] = [
        Screen::Now,
        Screen::Tree,
        Screen::Map,
        Screen::Tokens,
        Screen::Workers,
        Screen::Log,
    ];

    /// `--screen` spelling. Accepts the pane's column number too.
    pub fn parse(name: &str) -> Option<Screen> {
        parse_screen(name)
    }

    pub fn label(self) -> &'static str {
        match self {
            Screen::Now => "NOW",
            Screen::Tree => "TREE",
            Screen::Map => "MAP",
            Screen::Tokens => "TOKENS",
            Screen::Workers => "WORKERS",
            Screen::Log => "LOG",
        }
    }

    pub fn index(self) -> usize {
        self as usize
    }

    fn from_index(index: usize) -> Screen {
        Screen::ALL[index % SCREENS]
    }

    /// Wrap around, so Tab never dead-ends.
    fn step(self, delta: isize) -> Screen {
        let count = SCREENS as isize;
        Screen::from_index((self.index() as isize + delta).rem_euclid(count) as usize)
    }
}

#[derive(Clone)]
pub struct TuiConfig {
    pub title: String,
    pub threads: usize,
    pub turns: isize,
    pub seed: u64,
    /// 0 means unbounded.
    pub target: u64,
    pub checkpoint: String,
    pub screen: Screen,
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
    pub room: String,
    pub score: String,
}

struct Inner {
    cfg: TuiConfig,
    workers: Vec<WorkerStatus>,
    events: VecDeque<String>,
    games: u64,
    quit: bool,
    last_draw: Instant,
    screen: Screen,
    /// Scroll offset per pane; kept when switching away and back.
    scroll: [u16; SCREENS],
    /// Which worker the `Now` pane features.
    featured: usize,
    help: bool,
    /// Rows the last drawn pane held, so scrolling can stop at the end.
    pane_rows: usize,
    /// Lines the last drawn pane produced.
    content_len: usize,
}

struct Snapshot {
    cfg: TuiConfig,
    workers: Vec<WorkerStatus>,
    events: Vec<String>,
    games: u64,
    screen: Screen,
    scroll: [u16; SCREENS],
    featured: usize,
    help: bool,
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
        let screen = cfg.screen;
        let tui = Tui {
            inner: Arc::new(Mutex::new(Inner {
                cfg,
                workers: Vec::new(),
                events: VecDeque::new(),
                games: 0,
                quit: false,
                last_draw: Instant::now(),
                screen,
                scroll: [0; SCREENS],
                featured: 0,
                help: false,
                pane_rows: 1,
                content_len: 0,
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

    /// A callback the search calls once per turn. One per worker. Worker state
    /// is tracked even without a terminal, so `--screen X --no-ui` can print a
    /// real snapshot instead of an empty one.
    pub fn progress_callback(
        &self,
        id: usize,
    ) -> Option<Arc<dyn Fn(&Search, Progress) + Send + Sync>> {
        let inner = Arc::clone(&self.inner);
        Some(Arc::new(move |_search: &Search, progress: Progress| {
            if let Ok(mut guard) = inner.lock() {
                if let Some(worker) = guard.workers.get_mut(id) {
                    apply_progress(worker, progress);
                }
            }
        }))
    }

    /// Like [`Tui::progress_callback`], but also redraws and polls keys. The
    /// single-threaded driver only returns from `play_game` once per game, so
    /// without this the display would ignore input for a whole playthrough.
    pub fn live_callback(&self, id: usize) -> Option<Arc<dyn Fn(&Search, Progress) + Send + Sync>> {
        let inner = Arc::clone(&self.inner);
        let tui = self.clone();
        Some(Arc::new(move |search: &Search, progress: Progress| {
            if let Ok(mut guard) = inner.lock() {
                if let Some(worker) = guard.workers.get_mut(id) {
                    apply_progress(worker, progress);
                }
            }
            // A no-op when there is no terminal to draw on.
            tui.render(search);
        }))
    }

    /// Fold a finished game into the worker's row.
    pub fn game_done(&self, id: usize, search: &Search, end: GameEnd) {
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
                worker.room = search.current_room().to_string();
                worker.score = search.current_score().to_string();
            }
        }
    }

    pub fn push_events(&self, events: Vec<String>) {
        if events.is_empty() {
            return;
        }
        if let Ok(mut inner) = self.inner.lock() {
            inner.events.extend(events);
            while inner.events.len() > EVENT_ROWS {
                inner.events.pop_front();
            }
        }
    }

    /// The selected pane, rendered once as plain text. Used for terminal-free
    /// snapshots (`--screen X --no-ui`) and to keep the panes testable.
    pub fn frame_text(&self, master: &Search, width: usize, height: usize) -> Vec<String> {
        let snapshot = match self.snapshot_now() {
            Some(snapshot) => snapshot,
            None => return Vec::new(),
        };
        let (lines, _, _) = self.compose(master, &snapshot, height.max(6));
        lines
            .into_iter()
            .map(|(text, _, _)| fit(&text, width))
            .collect()
    }

    fn snapshot_now(&self) -> Option<Snapshot> {
        let inner = self.inner.lock().ok()?;
        Some(Snapshot {
            cfg: inner.cfg.clone(),
            workers: inner.workers.clone(),
            events: inner.events.iter().cloned().collect(),
            games: inner.games,
            screen: inner.screen,
            scroll: inner.scroll,
            featured: inner.featured,
            help: inner.help,
        })
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
                screen: inner.screen,
                scroll: inner.scroll,
                featured: inner.featured,
                help: inner.help,
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
                    if let Ok(mut inner) = self.inner.lock() {
                        apply_key(&mut inner, key);
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    }

    /// Assemble the frame: pinned header, tab strip, the scrolling pane, and
    /// the key hints. Returns the lines, how many the pane produced, and how
    /// many rows the pane had room for.
    fn compose(
        &self,
        master: &Search,
        snapshot: &Snapshot,
        height: usize,
    ) -> (Vec<Line>, usize, usize) {
        let mut lines: Vec<Line> = Vec::new();
        header_lines(&mut lines, self.start, snapshot, master);
        lines.push(tab_line(snapshot.screen));

        // One row at the bottom for the key hints.
        let pane_rows = height.saturating_sub(lines.len() + 1);
        let content = if snapshot.help {
            help_lines()
        } else {
            pane_lines(master, snapshot, pane_rows)
        };
        let content_len = content.len();
        let max_scroll = content_len.saturating_sub(pane_rows);
        let scroll = (snapshot.scroll[snapshot.screen.index()] as usize).min(max_scroll);
        lines.extend(content.into_iter().skip(scroll).take(pane_rows));
        while lines.len() + 1 < height {
            lines.push((String::new(), Color::Reset, false));
        }
        lines.push(footer_line());
        lines.truncate(height.max(1));
        (lines, content_len, pane_rows)
    }

    fn draw(&self, master: &Search, snapshot: &Snapshot) -> io::Result<()> {
        let (width, height) = terminal::size().unwrap_or((100, 30));
        let width = width.max(20) as usize;
        let height = height.max(6) as usize;

        let (lines, content_len, pane_rows) = self.compose(master, snapshot, height);

        let mut out = io::stdout();
        queue!(out, MoveTo(0, 0))?;
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
        out.flush()?;

        // Remember the pane size so the next keypress can clamp its scrolling.
        if let Ok(mut inner) = self.inner.lock() {
            inner.pane_rows = pane_rows;
            inner.content_len = content_len;
        }
        Ok(())
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        if self.owner {
            self.restore();
        }
    }
}

/// Keys are interpreted against view state only, so this is testable without a
/// terminal.
fn apply_key(inner: &mut Inner, key: KeyEvent) {
    let ctrl_c = key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'));
    if ctrl_c {
        inner.quit = true;
        return;
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => inner.quit = true,
        KeyCode::Char('?') => inner.help = !inner.help,
        KeyCode::Tab => inner.screen = inner.screen.step(1),
        KeyCode::BackTab => inner.screen = inner.screen.step(-1),
        KeyCode::Char(c @ '1'..='6') => {
            inner.screen = Screen::from_index(c as usize - '1' as usize)
        }
        KeyCode::Char('w') => inner.featured = inner.featured.wrapping_add(1),
        KeyCode::Down => inner.scroll_by(1),
        KeyCode::Up => inner.scroll_by(-1),
        KeyCode::PageDown => inner.scroll_by(10),
        KeyCode::PageUp => inner.scroll_by(-10),
        KeyCode::Home => inner.scroll_by(-(u16::MAX as isize)),
        KeyCode::End => inner.scroll_by(u16::MAX as isize),
        _ => {}
    }
}

impl Inner {
    fn scroll_by(&mut self, delta: isize) {
        let slot = &mut self.scroll[self.screen.index()];
        let max = self
            .content_len
            .saturating_sub(self.pane_rows)
            .min(u16::MAX as usize) as isize;
        *slot = (*slot as isize + delta).clamp(0, max) as u16;
    }
}

fn apply_progress(worker: &mut WorkerStatus, progress: Progress) {
    worker.turn = progress.turn;
    worker.goal = progress.goal;
    worker.cmd = progress.cmd;
    worker.seen = progress.tokens;
    worker.room = progress.room;
    worker.score = progress.score;
}

// ------------------------------------------------------------------ layout

fn header_lines(out: &mut Vec<Line>, start: Instant, snapshot: &Snapshot, master: &Search) {
    let target = if snapshot.cfg.target == 0 {
        "∞".to_string()
    } else {
        snapshot.cfg.target.to_string()
    };
    let elapsed = start.elapsed().as_secs_f64();
    let rate = if elapsed > 0.0 {
        snapshot.games as f64 / elapsed
    } else {
        0.0
    };
    let worker_vocab = snapshot
        .workers
        .iter()
        .map(|worker| worker.vocab)
        .max()
        .unwrap_or(0);

    out.push((
        format!(
            " zmfuzz · {} · {} turns · seed {}",
            snapshot.cfg.title, snapshot.cfg.turns, snapshot.cfg.seed
        ),
        Color::Cyan,
        true,
    ));
    out.push((
        format!(
            " {} threads · {}/{} games · {:.1}/s · {} · checkpoint {}",
            snapshot.cfg.threads,
            snapshot.games,
            target,
            rate,
            clock(start.elapsed().as_secs()),
            snapshot.cfg.checkpoint,
        ),
        Color::DarkGrey,
        false,
    ));
    // The pinned strip: whatever pane is selected, this is still visible.
    out.push((
        format!(
            " tok {}  vocab {}  stable {}  score {}  goals {}",
            master.token_count(),
            master.vocab_size().max(worker_vocab),
            master.stable_count(),
            master.top_score(),
            master.goal_queue().len()
        ),
        Color::White,
        false,
    ));
}

fn tab_line(current: Screen) -> Line {
    let mut text = String::from(" ");
    for screen in Screen::ALL {
        if screen == current {
            text.push_str(&format!("▸{}[{}] ", screen.label(), screen.index() + 1));
        } else {
            text.push_str(&format!(" {}[{}] ", screen.label(), screen.index() + 1));
        }
    }
    (text, Color::DarkGrey, false)
}

fn footer_line() -> Line {
    (
        " 1-6/Tab panes · ↑↓ PgUp/PgDn scroll · w worker · ? keys · q quit".to_string(),
        Color::DarkGrey,
        false,
    )
}

fn help_lines() -> Vec<Line> {
    let rows = [
        ("1 2 3 4 5 6", "select a pane directly"),
        ("Tab / Shift-Tab", "next / previous pane"),
        ("↑ ↓ PgUp PgDn Home End", "scroll the current pane"),
        ("w", "cycle the featured worker (NOW)"),
        ("?", "toggle this help"),
        ("q Esc Ctrl-C", "stop the search"),
    ];
    let mut out = vec![(" KEYS".to_string(), Color::White, true)];
    for (keys, what) in rows {
        out.push((format!("  {:24} {}", keys, what), Color::Reset, false));
    }
    out
}

fn pane_lines(master: &Search, snapshot: &Snapshot, rows: usize) -> Vec<Line> {
    match snapshot.screen {
        Screen::Now => now_lines(master, snapshot),
        Screen::Tree => tree_lines(master),
        Screen::Map => map_lines(snapshot),
        Screen::Tokens => token_lines(master, rows),
        Screen::Workers => worker_lines(snapshot),
        Screen::Log => log_lines(snapshot, rows),
    }
}

// -------------------------------------------------------------------- panes

fn now_lines(master: &Search, snapshot: &Snapshot) -> Vec<Line> {
    let mut out = Vec::new();
    let worker = if snapshot.workers.is_empty() {
        None
    } else {
        Some(&snapshot.workers[snapshot.featured % snapshot.workers.len()])
    };

    out.push((" NOW".to_string(), Color::White, true));
    match worker {
        Some(worker) => {
            let shown_turn = if snapshot.cfg.turns > 0 {
                worker.turn.min(snapshot.cfg.turns)
            } else {
                worker.turn
            };
            out.push((
                format!(
                    "  turns {} {}/{}   room {}   score {}",
                    bar(worker.turn, snapshot.cfg.turns, 20),
                    shown_turn.max(0),
                    snapshot.cfg.turns,
                    dash(&worker.room),
                    dash(&worker.score),
                ),
                Color::Green,
                false,
            ));
            out.push((
                format!("  goal {}   cmd \"{}\"", dash(&worker.goal), worker.cmd),
                Color::Reset,
                false,
            ));
            if !worker.seen.is_empty() {
                out.push((
                    format!("  saw  {}", worker.seen.join(" · ")),
                    Color::Cyan,
                    false,
                ));
            }
        }
        None => out.push((
            "  (no game in progress)".to_string(),
            Color::DarkGrey,
            false,
        )),
    }

    let route = master.current_route();
    if !route.is_empty() {
        out.push((String::new(), Color::Reset, false));
        out.push((" ROUTE (this game)".to_string(), Color::White, true));
        for (chunk, first) in chunk_route(&route, 8) {
            out.push((
                format!("  {}{}", if first { "" } else { "  " }, chunk.join(" → ")),
                Color::DarkGrey,
                false,
            ));
        }
    }

    if snapshot.workers.len() > 1 {
        out.push((String::new(), Color::Reset, false));
        out.push((" OTHER WORKERS".to_string(), Color::White, true));
        for (id, worker) in snapshot.workers.iter().enumerate() {
            if id == snapshot.featured % snapshot.workers.len() {
                continue;
            }
            out.push((
                format!(
                    "  #{:<2} {:>7} games  {:>4}t  {}  {}  \"{}\"",
                    id,
                    worker.games,
                    worker.turn,
                    pad(&dash(&worker.room), 18),
                    pad(&dash(&worker.score), 7),
                    worker.cmd,
                ),
                Color::DarkGrey,
                false,
            ));
        }
    }

    let recent = dedupe(snapshot.events.iter().rev().take(8).rev());
    if !recent.is_empty() {
        out.push((String::new(), Color::Reset, false));
        out.push((" RECENT".to_string(), Color::White, true));
        for (text, count) in recent {
            out.push((
                format!(
                    "  {}{}",
                    text,
                    if count > 1 {
                        format!(" ×{}", count)
                    } else {
                        String::new()
                    }
                ),
                event_color(&text),
                false,
            ));
        }
    }
    out
}

/// The command tree: the cheapest goals' best known paths, merged into a trie.
/// Shared prefixes collapse the flat goal queue into a few routes, which is the
/// question the goal list cannot answer: *how* does the search get there?
fn tree_lines(master: &Search) -> Vec<Line> {
    const GOALS: usize = 24;
    const MAX_LINES: usize = 400;

    let mut out = vec![(
        " TREE  cheapest first · ★ stable · ● this game's route".to_string(),
        Color::White,
        true,
    )];
    let queue = master.goal_queue();
    if queue.is_empty() {
        out.push((
            "  (no goals yet -- play some games)".to_string(),
            Color::DarkGrey,
            false,
        ));
        return out;
    }

    let mut goals = Vec::new();
    let mut orphans: Vec<Line> = Vec::new();
    let mut stable_goals = 0;
    for (priority, id) in queue.iter().take(GOALS) {
        let (first, runs, succ, stable, _score) = master.goal_detail(*id);
        let info = GoalInfo {
            name: master.token_name(*id).to_string(),
            priority: *priority,
            runs,
            succ,
            stable,
        };
        let _ = first;
        if stable {
            stable_goals += 1;
        }
        let path = master.best_path(*id);
        if path.is_empty() {
            orphans.push((
                format!(
                    "  (no path) {:10.3}  {}",
                    priority,
                    truncate(&info.name, 40)
                ),
                Color::DarkGrey,
                false,
            ));
        } else {
            goals.push((path, info));
        }
    }

    let route = master.current_route();
    let mut root = build_trie(&goals);
    sort_trie(&mut root);
    render_trie(&root, "", 0, &route, &mut out, MAX_LINES);

    out.push((String::new(), Color::Reset, false));
    out.push((
        format!(
            " {} goals ({} with a path, {} of these stable), showing {}",
            queue.len(),
            goals.len(),
            stable_goals,
            goals.len().min(GOALS)
        ),
        Color::DarkGrey,
        false,
    ));
    out.extend(orphans.into_iter().take(6));
    out
}

fn map_lines(snapshot: &Snapshot) -> Vec<Line> {
    let mut out = vec![(" MAP".to_string(), Color::White, true)];
    out.push((
        "  Not collected yet: the room graph needs object names (folly `Object`".to_string(),
        Color::DarkGrey,
        false,
    ));
    out.push((
        "  accessors) and room extraction from the Move events. Until then this".to_string(),
        Color::DarkGrey,
        false,
    ));
    out.push((
        "  pane shows only where each worker currently is.".to_string(),
        Color::DarkGrey,
        false,
    ));
    if snapshot.workers.is_empty() {
        return out;
    }
    out.push((String::new(), Color::Reset, false));
    out.push((" WORKER ROOMS".to_string(), Color::White, true));
    for (id, worker) in snapshot.workers.iter().enumerate() {
        out.push((
            format!(
                "  #{:<2} {}  {}",
                id,
                pad(&dash(&worker.room), 22),
                worker.score
            ),
            Color::Green,
            false,
        ));
    }
    out
}

fn token_lines(master: &Search, rows: usize) -> Vec<Line> {
    let mut out = vec![(
        " TOKENS  the goal queue, cheapest first".to_string(),
        Color::White,
        true,
    )];
    let queue = master.goal_queue();
    if queue.is_empty() {
        out.push((
            "  (nothing learned yet)".to_string(),
            Color::DarkGrey,
            false,
        ));
        return out;
    }
    out.push((
        "    priority  runs succ  first  stable  count  token".to_string(),
        Color::DarkGrey,
        false,
    ));
    let budget = rows.saturating_sub(2 + 8);
    for (priority, id) in queue.iter().take(budget.max(1)) {
        let (first, runs, succ, stable, _score) = master.goal_detail(*id);
        let count = master.stat(*id).count;
        out.push((
            format!(
                "  {:10.3}  {:>4} {:>4}  {:>5}  {:>6}  {:>5}  {}",
                priority,
                runs,
                succ,
                if first == crate::stat::FIRST_UNKNOWN {
                    "-".to_string()
                } else {
                    first.to_string()
                },
                if stable { "yes" } else { "no" },
                count,
                truncate(master.token_name(*id), 46),
            ),
            if stable { Color::Green } else { Color::Reset },
            false,
        ));
    }

    let stable = master.stabletok_ids();
    if !stable.is_empty() {
        out.push((String::new(), Color::Reset, false));
        out.push((
            format!(" STABLE ({}) in promotion order", stable.len()),
            Color::White,
            true,
        ));
        for id in stable.iter().take(8) {
            out.push((
                format!("  {}", truncate(master.token_name(*id), 60)),
                Color::Green,
                false,
            ));
        }
        if stable.len() > 8 {
            out.push((
                format!("  … {} more", stable.len() - 8),
                Color::DarkGrey,
                false,
            ));
        }
    }
    out
}

fn worker_lines(snapshot: &Snapshot) -> Vec<Line> {
    let mut out = vec![(" WORKERS".to_string(), Color::White, true)];
    if snapshot.workers.is_empty() {
        out.push(("  (none)".to_string(), Color::DarkGrey, false));
        return out;
    }
    for (id, worker) in snapshot.workers.iter().enumerate() {
        out.push((
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
        out.push((
            format!(
                "      goal {}  room {}  score {}  cmd \"{}\"",
                dash(&worker.goal),
                dash(&worker.room),
                dash(&worker.score),
                worker.cmd
            ),
            Color::DarkGrey,
            false,
        ));
    }
    out
}

fn log_lines(snapshot: &Snapshot, rows: usize) -> Vec<Line> {
    let mut out = vec![(
        " LOG  newest last, repeats collapsed".to_string(),
        Color::White,
        true,
    )];
    if snapshot.events.is_empty() {
        out.push(("  (quiet)".to_string(), Color::DarkGrey, false));
        return out;
    }
    let collapsed = dedupe(snapshot.events.iter());
    // Show the tail: the newest events are the interesting ones, and the pane
    // still scrolls from the top when the user asks for history.
    let skip = collapsed.len().saturating_sub(rows.saturating_sub(1));
    for (text, count) in collapsed.into_iter().skip(skip) {
        let mut line = format!("  {}", text);
        if count > 1 {
            line.push_str(&format!(" ×{}", count));
        }
        out.push((line, event_color(&text), false));
    }
    out
}

// ------------------------------------------------------------------- trie

#[derive(Clone)]
struct GoalInfo {
    name: String,
    priority: f64,
    runs: u64,
    succ: u64,
    stable: bool,
}

#[derive(Default)]
struct TrieNode {
    cmd: String,
    children: Vec<TrieNode>,
    goal: Option<GoalInfo>,
}

fn build_trie(paths: &[(Vec<String>, GoalInfo)]) -> TrieNode {
    let mut root = TrieNode::default();
    for (path, info) in paths {
        let mut node = &mut root;
        for cmd in path {
            let found = node.children.iter().position(|child| &child.cmd == cmd);
            let index = match found {
                Some(index) => index,
                None => {
                    node.children.push(TrieNode {
                        cmd: cmd.clone(),
                        ..TrieNode::default()
                    });
                    node.children.len() - 1
                }
            };
            node = &mut node.children[index];
        }
        node.goal = Some(info.clone());
    }
    root
}

/// Stable goals first: they are the ones worth showing when the tree is cut.
fn sort_trie(node: &mut TrieNode) {
    node.children.sort_by(|a, b| {
        let rank = |n: &TrieNode| match &n.goal {
            Some(goal) => (if goal.stable { 0 } else { 1 }, goal.priority),
            None => (2, f64::INFINITY),
        };
        rank(a)
            .partial_cmp(&rank(b))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for child in &mut node.children {
        sort_trie(child);
    }
}

fn render_trie(
    node: &TrieNode,
    prefix: &str,
    depth: usize,
    route: &[String],
    out: &mut Vec<Line>,
    limit: usize,
) {
    for (index, child) in node.children.iter().enumerate() {
        if out.len() >= limit {
            return;
        }
        let last = index + 1 == node.children.len();
        let on_route = route.get(depth).is_some_and(|cmd| *cmd == child.cmd);
        let mut text = format!(
            "  {}{}{} {}",
            prefix,
            if last { "└─" } else { "├─" },
            if on_route { "●" } else { " " },
            child.cmd
        );
        // Paths that do not branch (the common case: a goal reached by one
        // chain of commands) stay on one line instead of one line per command.
        let mut node = child;
        let mut level = depth;
        while node.goal.is_none() && node.children.len() == 1 {
            node = &node.children[0];
            level += 1;
            let on_route = route.get(level).is_some_and(|cmd| *cmd == node.cmd);
            text.push_str(&format!(
                " → {}{}",
                if on_route { "●" } else { "" },
                node.cmd
            ));
        }

        if let Some(goal) = &node.goal {
            text.push_str(&format!(
                "   {} {:10.3} runs={} succ={}",
                if goal.stable { "★" } else { " " },
                goal.priority,
                goal.runs,
                goal.succ
            ));
            text.push_str(&format!("  {}", truncate(&goal.name, 30)));
        }
        let color = match &node.goal {
            Some(goal) if goal.stable => Color::Green,
            Some(_) => Color::Reset,
            None => Color::DarkGrey,
        };
        out.push((text, color, false));
        let next = format!("{}{}", prefix, if last { "   " } else { "│  " });
        render_trie(node, &next, level + 1, route, out, limit);
    }
}

// ---------------------------------------------------------------- helpers

fn dash(text: &str) -> String {
    if text.is_empty() {
        "-".to_string()
    } else {
        text.to_string()
    }
}

fn pad(text: &str, width: usize) -> String {
    let mut out: String = text.chars().take(width).collect();
    while out.chars().count() < width {
        out.push(' ');
    }
    out
}

fn truncate(text: &str, width: usize) -> String {
    let mut out: String = text.chars().take(width).collect();
    if text.chars().count() > width {
        out.push('…');
    }
    out
}

/// `[####------]` for the turn budget. `max <= 0` means unbounded.
fn bar(turn: isize, max: isize, width: usize) -> String {
    if max <= 0 {
        return "-".to_string();
    }
    let done = (turn.max(0) as usize).min(max as usize);
    let filled = done * width / max as usize;
    let mut out = String::with_capacity(width + 2);
    out.push('[');
    for _ in 0..filled {
        out.push('#');
    }
    for _ in filled..width {
        out.push('-');
    }
    out.push(']');
    out
}

fn chunk_route(route: &[String], per_line: usize) -> Vec<(Vec<String>, bool)> {
    // Show the tail of a long game; the beginning is rarely what you want.
    let start = route.len().saturating_sub(per_line * 2);
    route[start..]
        .chunks(per_line)
        .enumerate()
        .map(|(index, chunk)| (chunk.to_vec(), index == 0))
        .collect()
}

/// Collapse consecutive duplicates into `(text, count)`.
fn dedupe<'a, I: IntoIterator<Item = &'a String>>(events: I) -> Vec<(String, usize)> {
    let mut out: Vec<(String, usize)> = Vec::new();
    for event in events {
        match out.last_mut() {
            Some((text, count)) if text == event => *count += 1,
            _ => out.push((event.clone(), 1)),
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    fn info(name: &str, priority: f64, stable: bool) -> GoalInfo {
        GoalInfo {
            name: name.to_string(),
            priority,
            runs: 1,
            succ: 1,
            stable,
        }
    }

    fn cmds(list: &[&str]) -> Vec<String> {
        list.iter().map(|c| c.to_string()).collect()
    }

    #[test]
    fn screens_parse_and_wrap() {
        assert_eq!(Screen::parse("tree"), Some(Screen::Tree));
        assert_eq!(Screen::parse("4"), Some(Screen::Tokens));
        assert_eq!(Screen::parse("nope"), None);
        assert_eq!(Screen::Now.step(-1), Screen::Log);
        assert_eq!(Screen::Log.step(1), Screen::Now);
    }

    #[test]
    fn trie_shares_prefixes_and_marks_goals() {
        let paths = vec![
            (
                cmds(&["north", "east", "open window"]),
                info("window is open", 1.0, true),
            ),
            (
                cmds(&["north", "east", "take sack"]),
                info("You take the sack", 2.0, false),
            ),
            (cmds(&["south"]), info("You are in the woods", 3.0, false)),
        ];
        let mut root = build_trie(&paths);
        sort_trie(&mut root);
        let mut out = Vec::new();
        render_trie(&root, "", 0, &cmds(&["north", "east"]), &mut out, 100);

        // "north" and its child "east" are shared, so each appears exactly once.
        let text: Vec<&str> = out.iter().map(|line| line.0.as_str()).collect();
        assert_eq!(text.iter().filter(|line| line.contains("north")).count(), 1);
        assert_eq!(text.iter().filter(|line| line.contains("east")).count(), 1);
        assert!(text.iter().any(|line| line.contains("open window")));
        assert!(text.iter().any(|line| line.contains("take sack")));
        // The live route is marked where it matches.
        assert!(text.iter().any(|line| line.contains("● north")));
        assert!(!out.is_empty());
    }

    #[test]
    fn trie_puts_stable_goals_first() {
        let paths = vec![
            (cmds(&["a"]), info("unstable", 0.1, false)),
            (cmds(&["b"]), info("stable", 9.0, true)),
        ];
        let mut root = build_trie(&paths);
        sort_trie(&mut root);
        assert_eq!(root.children[0].cmd, "b");
    }

    #[test]
    fn scroll_clamps_to_content() {
        let mut inner = Inner {
            cfg: TuiConfig {
                title: String::new(),
                threads: 1,
                turns: 10,
                seed: 1,
                target: 1,
                checkpoint: String::new(),
                screen: Screen::Now,
            },
            workers: Vec::new(),
            events: VecDeque::new(),
            games: 0,
            quit: false,
            last_draw: Instant::now(),
            screen: Screen::Now,
            scroll: [0; SCREENS],
            featured: 0,
            help: false,
            pane_rows: 5,
            content_len: 12,
        };
        inner.scroll_by(100);
        assert_eq!(inner.scroll[Screen::Now.index()], 7);
        inner.scroll_by(-100);
        assert_eq!(inner.scroll[Screen::Now.index()], 0);
    }

    #[test]
    fn keys_switch_panes_and_quit() {
        let mut inner = Inner {
            cfg: TuiConfig {
                title: String::new(),
                threads: 1,
                turns: 10,
                seed: 1,
                target: 1,
                checkpoint: String::new(),
                screen: Screen::Now,
            },
            workers: Vec::new(),
            events: VecDeque::new(),
            games: 0,
            quit: false,
            last_draw: Instant::now(),
            screen: Screen::Now,
            scroll: [0; SCREENS],
            featured: 0,
            help: false,
            pane_rows: 5,
            content_len: 5,
        };
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        apply_key(&mut inner, key(KeyCode::Char('3')));
        assert_eq!(inner.screen, Screen::Map);
        apply_key(&mut inner, key(KeyCode::Char('q')));
        assert!(inner.quit);
    }

    #[test]
    fn frame_keeps_the_header_and_tabs_above_the_pane() {
        let cfg = TuiConfig {
            title: "t.z3".to_string(),
            threads: 1,
            turns: 20,
            seed: 5,
            target: 3,
            checkpoint: "t.save".to_string(),
            screen: Screen::Tree,
        };
        let ui = Tui::new(false, cfg);
        // No story is parsed for layout, so a stub is enough.
        let search = Search::new(vec![3; 64], 1, 20, false);
        let frame = ui.frame_text(&search, 100, 12);

        assert_eq!(frame.len(), 12, "frame fills the height exactly");
        assert!(frame[0].contains("zmfuzz"));
        assert!(frame[0].contains("t.z3"));
        assert!(
            frame[2].contains("tok 0"),
            "knowledge strip is pinned: {}",
            frame[2]
        );
        assert!(
            frame[3].contains("▸TREE[2]"),
            "selected tab is marked: {}",
            frame[3]
        );
        assert!(frame[3].contains("NOW[1]") && frame[3].contains("LOG[6]"));
        assert!(
            frame[11].contains("q quit"),
            "footer is last: {}",
            frame[11]
        );
    }

    #[test]
    fn dedupe_collapses_repeats() {
        let events = vec![
            "A".to_string(),
            "A".to_string(),
            "B".to_string(),
            "A".to_string(),
        ];
        let out = dedupe(events.iter());
        assert_eq!(
            out,
            vec![
                ("A".to_string(), 2),
                ("B".to_string(), 1),
                ("A".to_string(), 1)
            ]
        );
    }
}

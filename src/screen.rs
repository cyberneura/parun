//! The full screen view of a run: one pane per worker, each showing the tail
//! of what its commands wrote, redrawn as the lines arrive, and scrolled back
//! through what came before on request.

use crate::{
    process,
    runner::{self, Event, Job, JobResult, Outcome},
    terminal::TerminalSession,
    text::{display_width, fit_to_width, plain_text, truncate_to_width},
};
use anyhow::Result;
use crossterm::{
    cursor::MoveTo,
    event::{
        self, Event as TerminalEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
    },
    queue,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{self, Clear, ClearType},
};
use std::{
    collections::VecDeque,
    io::Write,
    sync::mpsc::{self, TryRecvError},
    thread,
    time::{Duration, Instant},
};

/// Lines kept per pane. A pane shows its tail and can be scrolled back through
/// this much of what came before; a command that writes more than this has
/// said what it needed to say long before the end. The lines are kept as plain
/// text, so a pane full of them is a few hundred kilobytes at most for the
/// usual line length.
const PANE_HISTORY: usize = 2_000;
const MERGED_HISTORY: usize = 5_000;
/// Lines one notch of the mouse wheel scrolls.
const WHEEL_LINES: isize = 3;
/// A pane with fewer log rows than this shows too little to follow; below it,
/// the panes give way to one merged log.
const MIN_LOG_ROWS: usize = 3;
/// Frames are drawn no more often than this. A command writes lines faster
/// than a terminal draws frames, and redrawing on every line would only add
/// flicker.
const FRAME_INTERVAL: Duration = Duration::from_millis(50);
/// Events taken in before the keyboard is looked at again. Several commands
/// writing flat out can keep the channel from ever running empty, and a Ctrl-C
/// must not wait for that.
const EVENTS_PER_FRAME: usize = 2_000;
/// How long a stop waits for the commands to end on their own before killing
/// them, when the screen leaves with something still running.
const STOP_GRACE: Duration = Duration::from_secs(3);

/// What the user asked for while the screen was up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Abort {
    None,
    /// Ctrl-C once: the commands were asked to stop and can clean up.
    Requested,
    /// Ctrl-C twice: the commands were killed.
    Forced,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PaneState {
    Waiting,
    Running,
    Succeeded,
    Failed,
}

/// How far a view moves through the lines it keeps. Up is positive: towards
/// the oldest line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollBy {
    Lines(isize),
    Pages(isize),
    /// To the oldest line kept.
    Top,
    /// Back to the tail, which the view follows again from then on.
    Bottom,
}

/// What a key asks of the screen, besides stopping the commands or leaving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    /// The pane at this position among the open ones, counted from one.
    Select(usize),
    Next,
    Previous,
    /// Moves the active pane, or the merged log when that is what is shown.
    Scroll(ScrollBy),
}

/// A pane's log and where the view of it is: at the tail, or held at a line.
struct Log {
    lines: VecDeque<String>,
    capacity: usize,
    /// The index of the first line in view while the view is scrolled back;
    /// `None` follows the tail. An index rather than a distance from the tail,
    /// so that lines arriving below do not move what is being read.
    top: Option<usize>,
}

impl Log {
    fn new(capacity: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            capacity,
            top: None,
        }
    }

    fn push(&mut self, line: String) {
        if self.lines.len() == self.capacity {
            self.lines.pop_front();
            // The lines moved up by one; the view stays on the same ones until
            // the oldest kept line is the one in view, which then goes the way
            // of the ones before it.
            if let Some(top) = self.top {
                self.top = Some(top.saturating_sub(1));
            }
        }
        self.lines.push_back(line);
    }

    /// The index of the first line a view `rows` tall shows.
    fn start(&self, rows: usize) -> usize {
        let tail = self.lines.len().saturating_sub(rows);
        self.top.map_or(tail, |top| top.min(tail))
    }

    /// Lines below a view `rows` tall, when it is scrolled back.
    fn hidden_below(&self, rows: usize) -> usize {
        self.lines
            .len()
            .saturating_sub(self.start(rows).saturating_add(rows))
    }

    /// Lets a view `rows` tall go once it shows the tail. A view scrolled back
    /// comes to the tail without a key when it grows, after a resize or
    /// another pane closing; left as it was, it would follow the tail until the
    /// lines reached the line it was held at and then stop there, for no
    /// reason the user could see.
    fn settle(&mut self, rows: usize) {
        if self
            .top
            .is_some_and(|top| top >= self.lines.len().saturating_sub(rows))
        {
            self.top = None;
        }
    }

    /// Moves a view `rows` tall. Arriving at the tail lets the view follow it
    /// again.
    fn scroll(&mut self, by: ScrollBy, rows: usize) {
        let tail = self.lines.len().saturating_sub(rows);
        let page = rows.saturating_sub(1).max(1);
        let target = match by {
            ScrollBy::Lines(up) => moved(self.start(rows), up, tail),
            ScrollBy::Pages(up) => moved(self.start(rows), up.saturating_mul(as_isize(page)), tail),
            ScrollBy::Top => 0,
            ScrollBy::Bottom => tail,
        };
        self.top = (target < tail).then_some(target);
    }
}

/// `start` moved `up` lines towards the top, and no further down than `tail`.
fn moved(start: usize, up: isize, tail: usize) -> usize {
    if up >= 0 {
        start.saturating_sub(up.unsigned_abs())
    } else {
        start.saturating_add(up.unsigned_abs()).min(tail)
    }
}

fn as_isize(value: usize) -> isize {
    isize::try_from(value).unwrap_or(isize::MAX)
}

struct Pane {
    /// The job the worker holds or last held.
    job: Option<usize>,
    state: PaneState,
    log: Log,
    /// When the pane goes away, for a job that succeeded and was given a
    /// `--close-after`.
    close_at: Option<Instant>,
    /// Gone from the screen, leaving its rows to the others. The worker taking
    /// another job brings it back.
    closed: bool,
}

/// Where the body rows of a frame go.
enum Layout {
    /// The open panes, as worker indices in screen order, each `pane_rows`
    /// tall with `log_rows` of log under its title.
    Panes {
        open: Vec<usize>,
        pane_rows: usize,
        log_rows: usize,
    },
    /// The merged log, with this many rows under its title.
    Merged { log_rows: usize },
    /// Too short for anything but the footer.
    None,
}

pub struct Screen {
    names: Vec<String>,
    /// How long each job's pane stays once the job has succeeded, by job
    /// index; `None`, or no entry, keeps it.
    close_after: Vec<Option<Duration>>,
    /// The time the screen goes by, moved on by `advance`. Kept here rather
    /// than read from the system so that a frame depends on nothing else.
    now: Instant,
    panes: Vec<Pane>,
    /// The worker whose pane the keys scroll. Resolved against the open panes
    /// when it is used: a closed one hands over to the first open pane.
    active: usize,
    merged: Log,
    finished: Vec<JobResult>,
    abort: Abort,
    all_done: bool,
}

/// One row of a frame, as text and the colour it is drawn in.
#[derive(Debug, PartialEq, Eq)]
pub struct Row {
    text: String,
    color: Option<Color>,
    bold: bool,
}

impl Row {
    fn blank(width: usize) -> Self {
        Self {
            text: " ".repeat(width),
            color: None,
            bold: false,
        }
    }
}

impl Screen {
    pub fn new(names: Vec<String>) -> Self {
        Self {
            names,
            close_after: Vec::new(),
            now: Instant::now(),
            panes: Vec::new(),
            active: 0,
            merged: Log::new(MERGED_HISTORY),
            finished: Vec::new(),
            abort: Abort::None,
            all_done: false,
        }
    }

    /// Sets how long each job's pane stays once the job has succeeded.
    pub fn with_close_after(mut self, close_after: Vec<Option<Duration>>) -> Self {
        self.close_after = close_after;
        self
    }

    /// Moves the screen's clock on to `now` and closes the panes that are due.
    /// Answers whether the frame changed: a pane closed, or a countdown in a
    /// title went down by a second.
    pub fn advance(&mut self, now: Instant) -> bool {
        let before = self.now;
        self.now = now;
        let mut changed = false;
        for pane in &mut self.panes {
            let Some(close_at) = pane.close_at else {
                continue;
            };
            if now >= close_at {
                pane.close_at = None;
                pane.closed = true;
                changed = true;
            } else if seconds_left(close_at, before) != seconds_left(close_at, now) {
                changed = true;
            }
        }
        changed
    }

    fn name(&self, index: usize) -> &str {
        self.names.get(index).map_or("?", String::as_str)
    }

    pub fn apply(&mut self, event: Event) {
        match event {
            Event::Planned { workers } => {
                self.panes = (0..workers)
                    .map(|_| Pane {
                        job: None,
                        state: PaneState::Waiting,
                        log: Log::new(PANE_HISTORY),
                        close_at: None,
                        closed: false,
                    })
                    .collect();
            }
            Event::Started { worker, index } => {
                let separator = job_separator(self.name(index));
                let Some(pane) = self.panes.get_mut(worker) else {
                    return;
                };
                // The pane keeps the log of the jobs before this one, so the
                // new one is set off from them.
                if !pane.log.lines.is_empty() {
                    pane.log.push(String::new());
                    pane.log.push(separator);
                }
                pane.job = Some(index);
                pane.state = PaneState::Running;
                pane.close_at = None;
                pane.closed = false;
            }
            Event::Line {
                worker,
                index,
                line,
            } => {
                let line = plain_text(&line);
                let merged = format!("[{}] {line}", self.name(index));
                self.merged.push(merged);
                let Some(pane) = self.panes.get_mut(worker) else {
                    return;
                };
                pane.log.push(line);
            }
            Event::Finished {
                worker,
                index,
                result,
            } => {
                let closing = closing_line(&result);
                let merged = format!("[{}] {closing}", self.name(index));
                self.merged.push(merged);
                // Only a success goes away: a failure stays for its output.
                let close_after = self
                    .close_after
                    .get(index)
                    .copied()
                    .flatten()
                    .filter(|_| result.succeeded());
                let now = self.now;
                if let Some(pane) = self.panes.get_mut(worker) {
                    pane.state = if result.succeeded() {
                        PaneState::Succeeded
                    } else {
                        PaneState::Failed
                    };
                    pane.log.push(closing);
                    match close_after {
                        Some(delay) if delay.is_zero() => pane.closed = true,
                        // A delay too long for the clock to reach is as good as
                        // never, and `now + delay` would panic.
                        Some(delay) => pane.close_at = now.checked_add(delay),
                        None => {}
                    }
                }
                self.finished.push(result);
            }
        }
    }

    /// Lays the screen out as `height` rows of at most `width` columns each.
    /// Each row is padded to the full width, so drawing a frame over the last
    /// one needs no clearing, except the last row, which is left unpadded: a
    /// character in the bottom right corner makes some terminals scroll.
    pub fn frame(&self, width: usize, height: usize) -> Vec<Row> {
        let mut rows = Vec::with_capacity(height);
        let Some(body_rows) = height.checked_sub(1) else {
            return rows;
        };
        match self.layout(height) {
            Layout::Panes { open, log_rows, .. } => {
                let active = self.active_among(&open);
                // Numbers, and the mark on the active pane, mean something
                // only with a choice of panes.
                let numbered = open.len() > 1;
                for (position, &worker) in open.iter().enumerate() {
                    let pane = &self.panes[worker];
                    let active = Some(worker) == active;
                    let number = numbered.then_some(position + 1);
                    rows.push(title_row(
                        &self.pane_title(pane, number, active && numbered, log_rows),
                        width,
                        pane_color(pane),
                        active && numbered,
                    ));
                    rows.extend(log_rows_of(&pane.log, log_rows, width));
                }
            }
            Layout::Merged { log_rows } => {
                let title = if self.panes.is_empty() {
                    "parun  starting".to_owned()
                } else {
                    format!(
                        "parun  {} workers in one log: too small for panes{}",
                        self.panes.len(),
                        scrolled_suffix(&self.merged, log_rows)
                    )
                };
                rows.push(title_row(&title, width, Color::DarkCyan, false));
                rows.extend(log_rows_of(&self.merged, log_rows, width));
            }
            Layout::None => {}
        }
        while rows.len() < body_rows {
            rows.push(Row::blank(width));
        }
        rows.push(Row {
            text: truncate_to_width(&self.footer(height), width.saturating_sub(1)),
            color: Some(Color::DarkCyan),
            bold: false,
        });
        rows
    }

    /// Whether a screen `height` rows tall shows the panes, rather than the
    /// merged log. Decided by every pane, closed or not: closing is for the
    /// panes, and the merged log, where nothing closes, stays as it is.
    fn shows_panes(&self, height: usize) -> bool {
        let body_rows = height.saturating_sub(1);
        let pane_rows = body_rows.checked_div(self.panes.len()).unwrap_or(0);
        pane_rows.saturating_sub(1) >= MIN_LOG_ROWS
    }

    /// How a screen `height` rows tall is divided up.
    fn layout(&self, height: usize) -> Layout {
        let Some(body_rows) = height.checked_sub(1) else {
            return Layout::None;
        };
        if self.shows_panes(height) {
            // The panes still open share the rows; a closed one leaves its
            // rows to them.
            let open: Vec<usize> = (0..self.panes.len())
                .filter(|&worker| !self.panes[worker].closed)
                .collect();
            let pane_rows = body_rows.checked_div(open.len()).unwrap_or(0);
            Layout::Panes {
                open,
                pane_rows,
                log_rows: pane_rows.saturating_sub(1),
            }
        } else if body_rows > 0 {
            Layout::Merged {
                log_rows: body_rows - 1,
            }
        } else {
            Layout::None
        }
    }

    /// The worker whose pane the keys go to, among the `open` ones: the one
    /// chosen, or the first open pane once that has closed.
    fn active_among(&self, open: &[usize]) -> Option<usize> {
        if open.contains(&self.active) {
            Some(self.active)
        } else {
            open.first().copied()
        }
    }

    /// Lets go of the views that the layout of a screen `height` rows tall
    /// brings to the tail. Called before a frame is drawn, so that a view that
    /// grew with a resize, or with another pane closing, follows the tail
    /// from then on rather than stopping at the line it was once held at.
    pub fn settle(&mut self, height: usize) {
        match self.layout(height) {
            Layout::Panes { open, log_rows, .. } => {
                for worker in open {
                    self.panes[worker].log.settle(log_rows);
                }
            }
            Layout::Merged { log_rows } => self.merged.settle(log_rows),
            Layout::None => {}
        }
    }

    /// Does what a key asked for, on a screen `height` rows tall.
    pub fn navigate(&mut self, nav: Nav, height: usize) {
        match self.layout(height) {
            Layout::Panes { open, log_rows, .. } => {
                let Some(active) = self.active_among(&open) else {
                    return;
                };
                let position = open.iter().position(|&worker| worker == active);
                match nav {
                    Nav::Select(number) => {
                        if let Some(&worker) = number.checked_sub(1).and_then(|at| open.get(at)) {
                            self.active = worker;
                        }
                    }
                    Nav::Next => {
                        if let Some(position) = position {
                            self.active = open[(position + 1) % open.len()];
                        }
                    }
                    Nav::Previous => {
                        if let Some(position) = position {
                            self.active = open[(position + open.len() - 1) % open.len()];
                        }
                    }
                    Nav::Scroll(by) => {
                        self.active = active;
                        self.panes[active].log.scroll(by, log_rows);
                    }
                }
            }
            Layout::Merged { log_rows } => {
                if let Nav::Scroll(by) = nav {
                    self.merged.scroll(by, log_rows);
                }
            }
            Layout::None => {}
        }
    }

    /// Scrolls whatever is drawn on screen row `row`, and makes it the pane
    /// the keys go to. The mouse wheel comes here.
    pub fn scroll_at(&mut self, row: usize, by: ScrollBy, height: usize) {
        match self.layout(height) {
            Layout::Panes {
                open,
                pane_rows,
                log_rows,
            } => {
                // The rows left over below the last pane go with it.
                let Some(&worker) = row
                    .checked_div(pane_rows)
                    .and_then(|position| open.get(position.min(open.len().saturating_sub(1))))
                else {
                    return;
                };
                self.active = worker;
                self.panes[worker].log.scroll(by, log_rows);
            }
            Layout::Merged { log_rows } => self.merged.scroll(by, log_rows),
            Layout::None => {}
        }
    }

    /// Whether the screen can leave without a key press: every job has
    /// reported and every pane has closed, so there is nothing left to read.
    /// The merged log waits for a key as ever.
    pub fn is_finished_without_panes(&self, height: usize) -> bool {
        self.all_done
            && !self.panes.is_empty()
            && self.panes.iter().all(|pane| pane.closed)
            && self.shows_panes(height)
    }

    /// The title of a pane with `log_rows` under it: its number among the open
    /// panes and a mark when it is the active one, the job and how it stands,
    /// and how far back the view is when it is scrolled.
    fn pane_title(
        &self,
        pane: &Pane,
        number: Option<usize>,
        active: bool,
        log_rows: usize,
    ) -> String {
        let prefix = match (number, active) {
            (Some(number), true) => format!("> [{number}] "),
            (Some(number), false) => format!("[{number}] "),
            (None, _) => String::new(),
        };
        let suffix = scrolled_suffix(&pane.log, log_rows);
        let Some(index) = pane.job else {
            return format!("{prefix}waiting{suffix}");
        };
        let name = self.name(index);
        let standing = match pane.state {
            PaneState::Waiting | PaneState::Running => name.to_owned(),
            PaneState::Succeeded => match pane.close_at {
                Some(close_at) => format!(
                    "{name}  done  closes in {}s",
                    seconds_left(close_at, self.now)
                ),
                None => format!("{name}  done"),
            },
            PaneState::Failed => format!("{name}  FAILED"),
        };
        format!("{prefix}{standing}{suffix}")
    }

    fn footer(&self, height: usize) -> String {
        let done = self.finished.len();
        let total = self.names.len();
        let keys = match self.layout(height) {
            Layout::Panes { open, .. } if open.len() > 1 => "Up/Down scroll  1-9 pane",
            _ => "Up/Down scroll",
        };
        if self.all_done {
            let failed = self
                .finished
                .iter()
                .filter(|result| !result.succeeded())
                .count();
            let succeeded = done - failed;
            let missing = total - done;
            let aborted = if missing > 0 {
                format!(", {missing} not started")
            } else {
                String::new()
            };
            return format!("Done: {succeeded} ok, {failed} failed{aborted}    {keys}  Enter quit");
        }
        match self.abort {
            Abort::None => format!("{done}/{total} done    {keys}  Ctrl-C abort"),
            Abort::Requested => {
                format!(
                    "{done}/{total} done    aborting, waiting for commands to stop    Ctrl-C again to kill them"
                )
            }
            Abort::Forced => {
                format!("{done}/{total} done    killed, waiting for the workers to stop")
            }
        }
    }
}

/// What a title says about a view `rows` tall that is scrolled back: how many
/// lines lie below it. Nothing while the view is at the tail.
fn scrolled_suffix(log: &Log, rows: usize) -> String {
    match log.hidden_below(rows) {
        0 => String::new(),
        1 => "  scrolled: 1 line below".to_owned(),
        hidden => format!("  scrolled: {hidden} lines below"),
    }
}

/// What a key asks for, if it is one of the keys that move about the screen.
/// Up, `k` and Ctrl-P go up a line, Down, `j` and Ctrl-N down one; PageUp,
/// Ctrl-U and Ctrl-B a page up, PageDown, Ctrl-D and Ctrl-F a page down; Home
/// and `g` go to the oldest line, End and `G` back to the tail. `1` to `9`
/// pick a pane, Tab and Shift-Tab the next and the previous one.
pub fn navigation_for(key: KeyEvent) -> Option<Nav> {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let nav = match key.code {
        KeyCode::Up => Nav::Scroll(ScrollBy::Lines(1)),
        KeyCode::Down => Nav::Scroll(ScrollBy::Lines(-1)),
        KeyCode::Char('k') if !control => Nav::Scroll(ScrollBy::Lines(1)),
        KeyCode::Char('j') if !control => Nav::Scroll(ScrollBy::Lines(-1)),
        KeyCode::Char('p') if control => Nav::Scroll(ScrollBy::Lines(1)),
        KeyCode::Char('n') if control => Nav::Scroll(ScrollBy::Lines(-1)),
        KeyCode::PageUp => Nav::Scroll(ScrollBy::Pages(1)),
        KeyCode::PageDown => Nav::Scroll(ScrollBy::Pages(-1)),
        KeyCode::Char('u' | 'b') if control => Nav::Scroll(ScrollBy::Pages(1)),
        KeyCode::Char('d' | 'f') if control => Nav::Scroll(ScrollBy::Pages(-1)),
        KeyCode::Home => Nav::Scroll(ScrollBy::Top),
        KeyCode::Char('g') if !control => Nav::Scroll(ScrollBy::Top),
        KeyCode::End => Nav::Scroll(ScrollBy::Bottom),
        KeyCode::Char('G') if !control => Nav::Scroll(ScrollBy::Bottom),
        KeyCode::Tab => Nav::Next,
        KeyCode::BackTab => Nav::Previous,
        KeyCode::Char(digit @ '1'..='9') if !control => {
            Nav::Select(digit.to_digit(10).map_or(0, |digit| digit as usize))
        }
        _ => return None,
    };
    Some(nav)
}

/// Whole seconds from `now` to `close_at`, rounded up, so that a countdown
/// shows `1s` until the moment the pane goes.
fn seconds_left(close_at: Instant, now: Instant) -> u128 {
    close_at
        .saturating_duration_since(now)
        .as_millis()
        .div_ceil(1000)
}

fn pane_color(pane: &Pane) -> Color {
    match pane.state {
        PaneState::Waiting | PaneState::Running => Color::DarkCyan,
        PaneState::Succeeded => Color::Green,
        PaneState::Failed => Color::Red,
    }
}

/// The line that opens a job's part of a pane. It starts with a rule so that it
/// can be told from output: no command starts a line with one.
fn job_separator(name: &str) -> String {
    format!("\u{2500}\u{2500} {name}")
}

/// The line that closes a job's part of a pane, with how it ended.
fn closing_line(result: &JobResult) -> String {
    let elapsed = format_duration(result.elapsed);
    match &result.outcome {
        Outcome::NotRun(message) => format!("\u{2500}\u{2500} not run: {message}"),
        outcome => format!("\u{2500}\u{2500} {} ({elapsed})", outcome.label()),
    }
}

/// `0.3s`, `12.5s`, `3m05s`, `1h02m`.
pub fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs_f64();
    if seconds < 60.0 {
        return format!("{seconds:.1}s");
    }
    let whole = duration.as_secs();
    if whole < 3600 {
        return format!("{}m{:02}s", whole / 60, whole % 60);
    }
    format!("{}h{:02}m", whole / 3600, (whole % 3600) / 60)
}

/// A rule with the title set into it, the way a pane border is drawn.
fn title_row(title: &str, width: usize, color: Color, bold: bool) -> Row {
    let label = truncate_to_width(&format!(" {title} "), width.saturating_sub(2));
    let rule = "\u{2500}".repeat(width.saturating_sub(display_width(&label) + 2));
    Row {
        text: fit_to_width(&format!("\u{2500}\u{2500}{label}{rule}"), width),
        color: Some(color),
        bold,
    }
}

/// The `count` lines in view, the tail unless the log is scrolled back, each
/// cut to the width rather than wrapped: a wrapped line would take a row from
/// the lines above it, and a log is read for what happened, not for every
/// column of it.
fn log_rows_of(log: &Log, count: usize, width: usize) -> Vec<Row> {
    let mut rows: Vec<Row> = log
        .lines
        .iter()
        .skip(log.start(count))
        .take(count)
        .map(|line| Row {
            text: fit_to_width(line, width),
            color: line_color(line),
            bold: false,
        })
        .collect();
    while rows.len() < count {
        rows.push(Row::blank(width));
    }
    rows
}

fn line_color(line: &str) -> Option<Color> {
    line.starts_with('\u{2500}').then_some(Color::DarkGrey)
}

fn draw(stdout: &mut impl Write, screen: &Screen, width: usize, height: usize) -> Result<()> {
    let rows = screen.frame(width, height);
    let last = rows.len().saturating_sub(1);
    for (index, row) in rows.iter().enumerate() {
        queue!(stdout, MoveTo(0, u16::try_from(index).unwrap_or(u16::MAX)))?;
        if index == last {
            queue!(stdout, Clear(ClearType::CurrentLine))?;
        }
        if let Some(color) = row.color {
            queue!(stdout, SetForegroundColor(color))?;
        }
        if row.bold {
            queue!(stdout, SetAttribute(Attribute::Bold))?;
        }
        queue!(
            stdout,
            Print(&row.text),
            SetAttribute(Attribute::Reset),
            ResetColor
        )?;
    }
    stdout.flush()?;
    Ok(())
}

/// What the screen ends with: the results of the jobs that ran, and whether
/// the user aborted before all of them had.
pub struct ScreenOutcome {
    pub results: Vec<Option<JobResult>>,
    pub aborted: bool,
}

/// Runs the jobs on a background thread and shows them until they end and a
/// key is pressed. The commands run in process groups of their own: the
/// terminal is in raw mode, so Ctrl-C arrives here as a key press, and the
/// screen has to pass it on itself. Should the screen itself fail, because the
/// terminal went away, the commands are stopped before the error is returned;
/// nothing else would be left to stop them.
pub fn run(
    jobs: Vec<Job>,
    concurrency: usize,
    close_after: Vec<Option<Duration>>,
) -> Result<ScreenOutcome> {
    let mut terminal = TerminalSession::enter()?;
    let mut screen = Screen::new(jobs.iter().map(|job| job.name.clone()).collect())
        .with_close_after(close_after);
    // Bounded like the channel inside `run_jobs`, and for the same reason.
    let (tx, rx) = mpsc::sync_channel(runner::EVENT_QUEUE);
    let worker = thread::spawn(move || {
        runner::run_jobs(&jobs, concurrency, |event| {
            let _ = tx.send(event);
        })
    });
    let watched = watch(&mut terminal, &mut screen, &rx);
    // Nothing drains the channel from here on. A sender blocked on it would
    // keep the worker thread, and the join below, waiting for good.
    drop(rx);
    match &watched {
        // The run is over; what is still listed is something a command left
        // behind, which would otherwise outlive parun.
        Ok(()) => process::stop_leftover_commands(STOP_GRACE),
        // The screen is gone with commands still running and nothing left to
        // press Ctrl-C at.
        Err(_) => process::stop_running_commands(STOP_GRACE),
    }
    drop(terminal);
    let results = worker
        .join()
        .map_err(|_| anyhow::anyhow!("the worker thread panicked"))?;
    watched?;
    let aborted = screen.abort != Abort::None;
    Ok(ScreenOutcome { results, aborted })
}

/// Draws the screen and answers the keyboard until every worker has reported
/// and a key has been pressed, or until every pane has closed. The keys that
/// scroll and pick a pane are answered throughout, and once everything is done
/// they are the keys that do not leave the screen: the output is there to be
/// read back. The keyboard is polled with a timeout of one frame, so the loop
/// comes round at least that often even with nothing happening: that is what
/// moves the close countdowns on and closes the panes that are due.
fn watch(
    terminal: &mut TerminalSession,
    screen: &mut Screen,
    rx: &mpsc::Receiver<Event>,
) -> Result<()> {
    let mut dirty = true;
    let mut last_frame = Instant::now() - FRAME_INTERVAL;
    loop {
        if screen.advance(Instant::now()) {
            dirty = true;
        }
        for _ in 0..EVENTS_PER_FRAME {
            match rx.try_recv() {
                Ok(event) => {
                    screen.apply(event);
                    dirty = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if !screen.all_done {
                        screen.all_done = true;
                        dirty = true;
                    }
                    break;
                }
            }
        }
        if dirty && (screen.all_done || last_frame.elapsed() >= FRAME_INTERVAL) {
            let (width, height) = terminal::size()?;
            screen.settle(usize::from(height));
            draw(
                &mut terminal.stdout,
                screen,
                usize::from(width),
                usize::from(height),
            )?;
            dirty = false;
            last_frame = Instant::now();
        }
        if screen.all_done {
            let (_, height) = terminal::size()?;
            if screen.is_finished_without_panes(usize::from(height)) {
                return Ok(());
            }
        }
        if !event::poll(FRAME_INTERVAL)? {
            continue;
        }
        let key = match event::read()? {
            TerminalEvent::Key(key) => key,
            TerminalEvent::Mouse(mouse) => {
                let by = match mouse.kind {
                    MouseEventKind::ScrollUp => ScrollBy::Lines(WHEEL_LINES),
                    MouseEventKind::ScrollDown => ScrollBy::Lines(-WHEEL_LINES),
                    _ => continue,
                };
                let (_, height) = terminal::size()?;
                screen.scroll_at(usize::from(mouse.row), by, usize::from(height));
                dirty = true;
                continue;
            }
            TerminalEvent::Resize(..) => {
                dirty = true;
                continue;
            }
            _ => continue,
        };
        // A held key repeats, and two Ctrl-C from one press would kill the
        // commands outright when the user meant to ask them to stop once.
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if let Some(nav) = navigation_for(key) {
            let (_, height) = terminal::size()?;
            screen.navigate(nav, usize::from(height));
            dirty = true;
            continue;
        }
        if screen.all_done {
            return Ok(());
        }
        let is_ctrl_c =
            key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
        if !is_ctrl_c {
            continue;
        }
        match screen.abort {
            Abort::None => {
                process::interrupt_running_commands();
                screen.abort = Abort::Requested;
            }
            // A command that started in the moment between the kill and its
            // registration was not on the list then; a further Ctrl-C reaches
            // it, rather than being ignored.
            Abort::Requested | Abort::Forced => {
                process::terminate_running_commands();
                screen.abort = Abort::Forced;
            }
        }
        dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::ExitStatus;

    fn planned(workers: usize, names: &[&str]) -> Screen {
        let mut screen = Screen::new(names.iter().map(|name| (*name).to_owned()).collect());
        screen.apply(Event::Planned { workers });
        screen
    }

    fn succeeded() -> JobResult {
        JobResult {
            outcome: Outcome::Succeeded,
            output: String::new(),
            elapsed: Duration::from_millis(1234),
        }
    }

    #[cfg(unix)]
    fn failed(code: i32) -> JobResult {
        use std::os::unix::process::ExitStatusExt;
        JobResult {
            outcome: Outcome::Failed(ExitStatus::from_raw(code << 8)),
            output: String::new(),
            elapsed: Duration::from_millis(500),
        }
    }

    fn texts(rows: &[Row]) -> Vec<&str> {
        rows.iter().map(|row| row.text.as_str()).collect()
    }

    #[test]
    fn splits_the_screen_into_one_pane_per_worker() {
        // Arrange
        let mut screen = planned(4, &["a", "b", "build", "d", "e", "f"]);
        screen.apply(Event::Started {
            worker: 2,
            index: 2,
        });
        screen.apply(Event::Line {
            worker: 2,
            index: 2,
            line: "compiling".into(),
        });

        // Act
        let rows = screen.frame(100, 41);

        // Assert: 40 body rows make four panes of ten, then the footer.
        assert_eq!(rows.len(), 41);
        let texts = texts(&rows);
        assert!(texts[0].contains(" waiting "), "{}", texts[0]);
        assert!(texts[10].contains(" waiting "), "{}", texts[10]);
        assert!(texts[20].contains(" build "), "{}", texts[20]);
        assert!(texts[21].starts_with("compiling"), "{}", texts[21]);
        assert!(texts[30].contains(" waiting "), "{}", texts[30]);
        assert!(texts[40].contains("0/6 done"), "{}", texts[40]);
        for row in &rows[..40] {
            assert_eq!(display_width(&row.text), 100, "{}", row.text);
        }
    }

    #[test]
    fn falls_back_to_one_merged_log_when_the_panes_would_be_too_short() {
        // Arrange: ten workers on 24 rows leave two rows per pane.
        let names: Vec<String> = (0..10).map(|i| format!("job-{i}")).collect();
        let mut screen = Screen::new(names);
        screen.apply(Event::Planned { workers: 10 });
        for worker in 0..10 {
            screen.apply(Event::Line {
                worker,
                index: worker,
                line: "working".into(),
            });
        }

        // Act
        let rows = screen.frame(80, 24);

        // Assert
        assert_eq!(rows.len(), 24);
        let texts = texts(&rows);
        assert!(texts[0].contains("too small for panes"), "{}", texts[0]);
        assert!(texts[1].starts_with("[job-0] working"));
        assert!(texts[10].starts_with("[job-9] working"));
    }

    #[test]
    fn shows_the_tail_of_a_pane_and_cuts_wide_lines_at_the_edge() {
        // Arrange: one worker, a 12 row screen, so the pane has 10 log rows.
        let mut screen = planned(1, &["job"]);
        screen.apply(Event::Started {
            worker: 0,
            index: 0,
        });
        for index in 0..30 {
            screen.apply(Event::Line {
                worker: 0,
                index: 0,
                line: format!("line {index} 日本語の長い出力がここに続きます"),
            });
        }

        // Act
        let rows = screen.frame(30, 12);

        // Assert
        let texts = texts(&rows);
        assert!(texts[1].starts_with("line 20 "), "{}", texts[1]);
        assert!(texts[10].starts_with("line 29 "), "{}", texts[10]);
        for row in &rows {
            assert!(display_width(&row.text) <= 30, "{}", row.text);
        }
    }

    #[test]
    fn strips_escape_sequences_out_of_the_lines_it_keeps() {
        // Arrange
        let mut screen = planned(1, &["job"]);
        screen.apply(Event::Line {
            worker: 0,
            index: 0,
            line: "\u{1b}[32mgreen\u{1b}[0m text".into(),
        });

        // Act
        let rows = screen.frame(40, 6);

        // Assert
        assert!(rows[1].text.starts_with("green text"), "{}", rows[1].text);
        assert_eq!(display_width(&rows[1].text), 40);
    }

    #[cfg(unix)]
    #[test]
    fn colours_the_titles_by_how_the_job_ended() {
        // Arrange
        let mut screen = planned(2, &["good", "bad"]);
        screen.apply(Event::Started {
            worker: 0,
            index: 0,
        });
        screen.apply(Event::Started {
            worker: 1,
            index: 1,
        });
        screen.apply(Event::Finished {
            worker: 0,
            index: 0,
            result: succeeded(),
        });
        screen.apply(Event::Finished {
            worker: 1,
            index: 1,
            result: failed(1),
        });

        // Act
        let rows = screen.frame(60, 11);

        // Assert: five rows per pane, so the titles sit on rows 0 and 5.
        assert!(rows[0].text.contains(" good  done "), "{}", rows[0].text);
        assert_eq!(rows[0].color, Some(Color::Green));
        assert!(
            rows[1].text.starts_with("\u{2500}\u{2500} ok (1.2s)"),
            "{}",
            rows[1].text
        );
        assert_eq!(rows[1].color, Some(Color::DarkGrey));
        assert!(rows[5].text.contains(" bad  FAILED "), "{}", rows[5].text);
        assert_eq!(rows[5].color, Some(Color::Red));
        assert!(
            rows[6].text.starts_with("\u{2500}\u{2500} exit 1 (0.5s)"),
            "{}",
            rows[6].text
        );
    }

    #[cfg(unix)]
    #[test]
    fn footer_counts_the_outcomes_once_everything_has_run() {
        // Arrange
        let mut screen = planned(2, &["a", "b", "c", "d"]);
        screen.apply(Event::Finished {
            worker: 0,
            index: 0,
            result: succeeded(),
        });
        screen.apply(Event::Finished {
            worker: 1,
            index: 1,
            result: failed(2),
        });
        screen.apply(Event::Finished {
            worker: 0,
            index: 2,
            result: succeeded(),
        });
        let running = screen.footer(21);
        screen.all_done = true;

        // Act
        let done = screen.footer(21);

        // Assert: the fourth job never started, as after an abort.
        assert_eq!(
            running,
            "3/4 done    Up/Down scroll  1-9 pane  Ctrl-C abort"
        );
        assert_eq!(
            done,
            "Done: 2 ok, 1 failed, 1 not started    Up/Down scroll  1-9 pane  Enter quit"
        );
    }

    #[test]
    fn the_footer_offers_no_pane_keys_with_one_pane_or_the_merged_log() {
        // Arrange: one worker on a tall screen; ten workers on a short one.
        let one = planned(1, &["a"]);
        let names: Vec<String> = (0..10).map(|i| format!("job-{i}")).collect();
        let mut many = Screen::new(names);
        many.apply(Event::Planned { workers: 10 });

        // Act / Assert
        assert_eq!(one.footer(21), "0/1 done    Up/Down scroll  Ctrl-C abort");
        assert_eq!(many.footer(24), "0/10 done    Up/Down scroll  Ctrl-C abort");
    }

    /// One worker on a 12 row screen (10 log rows) that wrote `count` lines
    /// `line 0` to `line count-1`.
    fn one_pane_with_lines(count: usize) -> Screen {
        let mut screen = planned(1, &["job"]);
        screen.apply(Event::Started {
            worker: 0,
            index: 0,
        });
        for index in 0..count {
            screen.apply(Event::Line {
                worker: 0,
                index: 0,
                line: format!("line {index}"),
            });
        }
        screen
    }

    fn first_log_row(screen: &Screen) -> String {
        screen.frame(40, 12)[1].text.trim_end().to_owned()
    }

    #[test]
    fn scrolls_a_pane_back_and_holds_it_there_as_lines_arrive() {
        // Arrange
        let mut screen = one_pane_with_lines(30);

        // Act
        screen.navigate(Nav::Scroll(ScrollBy::Lines(1)), 12);
        let up_one = first_log_row(&screen);
        screen.navigate(Nav::Scroll(ScrollBy::Pages(1)), 12);
        let up_a_page = first_log_row(&screen);
        let title = screen.frame(40, 12)[0].text.clone();
        screen.apply(Event::Line {
            worker: 0,
            index: 0,
            line: "line 30".into(),
        });
        let after_a_line = first_log_row(&screen);

        // Assert: a page is a row short of the view, so that one line carries
        // over, and the new line below does not move the view.
        assert_eq!(up_one, "line 19");
        assert_eq!(up_a_page, "line 10");
        assert!(title.contains(" job  scrolled: 10 lines below "), "{title}");
        assert_eq!(after_a_line, "line 10");
    }

    #[test]
    fn scrolling_down_to_the_tail_follows_it_again() {
        // Arrange
        let mut screen = one_pane_with_lines(30);
        screen.navigate(Nav::Scroll(ScrollBy::Top), 12);
        let at_top = first_log_row(&screen);

        // Act
        screen.navigate(Nav::Scroll(ScrollBy::Pages(-1)), 12);
        screen.navigate(Nav::Scroll(ScrollBy::Lines(-20)), 12);
        let back = first_log_row(&screen);
        screen.apply(Event::Line {
            worker: 0,
            index: 0,
            line: "line 30".into(),
        });
        let after_a_line = first_log_row(&screen);
        let title = screen.frame(40, 12)[0].text.clone();

        // Assert
        assert_eq!(at_top, "line 0");
        assert_eq!(back, "line 20");
        assert_eq!(after_a_line, "line 21");
        assert!(!title.contains("scrolled"), "{title}");
    }

    #[test]
    fn end_leaves_a_scrolled_pane_following_the_tail() {
        // Arrange
        let mut screen = one_pane_with_lines(30);
        screen.navigate(Nav::Scroll(ScrollBy::Top), 12);

        // Act
        screen.navigate(Nav::Scroll(ScrollBy::Bottom), 12);

        // Assert
        assert_eq!(first_log_row(&screen), "line 20");
        assert_eq!(screen.panes[0].log.top, None);
    }

    #[test]
    fn a_view_scrolled_back_stays_on_its_lines_as_the_oldest_are_dropped() {
        // Arrange: the pane is full, and the view is held a page from the end.
        let mut screen = one_pane_with_lines(PANE_HISTORY);
        screen.navigate(Nav::Scroll(ScrollBy::Pages(1)), 12);
        let before = first_log_row(&screen);

        // Act
        for index in PANE_HISTORY..PANE_HISTORY + 5 {
            screen.apply(Event::Line {
                worker: 0,
                index: 0,
                line: format!("line {index}"),
            });
        }

        // Assert
        assert_eq!(before, format!("line {}", PANE_HISTORY - 19));
        assert_eq!(first_log_row(&screen), before);
        assert_eq!(screen.panes[0].log.lines.len(), PANE_HISTORY);
    }

    #[test]
    fn a_view_brought_to_the_tail_by_a_taller_pane_follows_it_from_then_on() {
        // Arrange: held a line up on 10 rows, then drawn on 15.
        let mut screen = one_pane_with_lines(30);
        screen.navigate(Nav::Scroll(ScrollBy::Lines(1)), 12);
        let held = screen.panes[0].log.top;

        // Act
        screen.settle(17);
        for index in 30..40 {
            screen.apply(Event::Line {
                worker: 0,
                index: 0,
                line: format!("line {index}"),
            });
        }
        let rows = screen.frame(40, 17);

        // Assert: without the settling, the view would have stopped at line 19.
        assert_eq!(held, Some(19));
        assert_eq!(screen.panes[0].log.top, None);
        assert!(rows[1].text.starts_with("line 25 "), "{}", rows[1].text);
        assert!(!rows[0].text.contains("scrolled"), "{}", rows[0].text);
    }

    #[test]
    fn settling_keeps_a_view_that_is_still_scrolled_back() {
        // Arrange
        let mut screen = one_pane_with_lines(30);
        screen.navigate(Nav::Scroll(ScrollBy::Top), 12);

        // Act
        screen.settle(12);
        screen.settle(17);

        // Assert
        assert_eq!(screen.panes[0].log.top, Some(0));
    }

    #[test]
    fn a_short_log_scrolls_nowhere() {
        // Arrange
        let mut screen = one_pane_with_lines(3);

        // Act
        screen.navigate(Nav::Scroll(ScrollBy::Lines(1)), 12);
        screen.navigate(Nav::Scroll(ScrollBy::Top), 12);

        // Assert
        assert_eq!(first_log_row(&screen), "line 0");
        assert_eq!(screen.panes[0].log.top, None);
    }

    /// Three workers, each on its own job, with ten lines in every pane.
    fn three_panes() -> Screen {
        let mut screen = planned(3, &["a", "b", "c"]);
        for worker in 0..3 {
            screen.apply(Event::Started {
                worker,
                index: worker,
            });
            for index in 0..10 {
                screen.apply(Event::Line {
                    worker,
                    index: worker,
                    line: format!("{} {index}", ["a", "b", "c"][worker]),
                });
            }
        }
        screen
    }

    fn titles(screen: &Screen, height: usize) -> Vec<String> {
        let rows = screen.frame(60, height);
        let pane_rows = (height - 1) / 3;
        (0..3)
            .map(|position| rows[position * pane_rows].text.clone())
            .collect()
    }

    #[test]
    fn numbers_the_panes_and_marks_the_one_the_keys_go_to() {
        // Arrange
        let mut screen = three_panes();
        let at_first = titles(&screen, 16);

        // Act
        screen.navigate(Nav::Select(3), 16);
        let third = titles(&screen, 16);
        screen.navigate(Nav::Next, 16);
        let wrapped = titles(&screen, 16);
        screen.navigate(Nav::Previous, 16);
        let back = titles(&screen, 16);
        screen.navigate(Nav::Select(9), 16);
        let out_of_range = titles(&screen, 16);

        // Assert
        assert!(at_first[0].contains(" > [1] a "), "{}", at_first[0]);
        assert!(at_first[1].contains(" [2] b "), "{}", at_first[1]);
        assert!(third[2].contains(" > [3] c "), "{}", third[2]);
        assert!(wrapped[0].contains(" > [1] a "), "{}", wrapped[0]);
        assert!(back[2].contains(" > [3] c "), "{}", back[2]);
        assert_eq!(out_of_range, back);
        let rows = screen.frame(60, 16);
        assert!(rows[10].bold);
        assert!(!rows[0].bold);
    }

    #[test]
    fn a_single_pane_is_neither_numbered_nor_marked() {
        // Arrange
        let screen = one_pane_with_lines(1);

        // Act
        let rows = screen.frame(40, 12);

        // Assert
        assert!(
            rows[0].text.starts_with("\u{2500}\u{2500} job "),
            "{}",
            rows[0].text
        );
        assert!(!rows[0].bold);
    }

    #[test]
    fn keys_scroll_the_active_pane_and_leave_the_others() {
        // Arrange: 16 rows give three panes of five, four log rows each.
        let mut screen = three_panes();
        screen.navigate(Nav::Select(2), 16);

        // Act
        screen.navigate(Nav::Scroll(ScrollBy::Lines(2)), 16);
        let rows = screen.frame(60, 16);

        // Assert
        assert!(rows[1].text.starts_with("a 6 "), "{}", rows[1].text);
        assert!(rows[6].text.starts_with("b 4 "), "{}", rows[6].text);
        assert!(rows[11].text.starts_with("c 6 "), "{}", rows[11].text);
        assert!(
            rows[5].text.contains(" > [2] b  scrolled: 2 lines below "),
            "{}",
            rows[5].text
        );
    }

    #[test]
    fn the_wheel_scrolls_the_pane_under_it_and_makes_it_the_active_one() {
        // Arrange: panes of five rows sit at rows 0-4, 5-9 and 10-14.
        let mut screen = three_panes();

        // Act
        screen.scroll_at(12, ScrollBy::Lines(3), 16);
        let rows = screen.frame(60, 16);

        // Assert: the rows below the last pane, 15 here, go with it.
        assert!(rows[11].text.starts_with("c 3 "), "{}", rows[11].text);
        assert!(rows[1].text.starts_with("a 6 "), "{}", rows[1].text);
        assert!(rows[10].text.contains(" > [3] c "), "{}", rows[10].text);
        screen.scroll_at(15, ScrollBy::Lines(1), 16);
        assert!(
            screen.frame(60, 16)[11].text.starts_with("c 2 "),
            "{}",
            screen.frame(60, 16)[11].text
        );
    }

    #[test]
    fn a_closed_active_pane_hands_the_keys_to_the_first_open_one() {
        // Arrange: the first pane closes; the keys were on it.
        let mut screen =
            planned(3, &["a", "b", "c"]).with_close_after(vec![Some(Duration::ZERO); 3]);
        for worker in 0..3 {
            screen.apply(Event::Started {
                worker,
                index: worker,
            });
        }
        finish(&mut screen, 0, succeeded());

        // Act
        let rows = screen.frame(60, 21);

        // Assert: two panes of ten; the second job's pane is first and marked.
        assert!(rows[0].text.contains(" > [1] b "), "{}", rows[0].text);
        assert!(rows[10].text.contains(" [2] c "), "{}", rows[10].text);
    }

    #[test]
    fn the_merged_log_scrolls_as_one() {
        // Arrange: ten workers on 24 rows give the merged log, 22 log rows.
        let names: Vec<String> = (0..10).map(|i| format!("job-{i}")).collect();
        let mut screen = Screen::new(names);
        screen.apply(Event::Planned { workers: 10 });
        for index in 0..40 {
            screen.apply(Event::Line {
                worker: index % 10,
                index: index % 10,
                line: format!("line {index}"),
            });
        }

        // Act
        screen.navigate(Nav::Select(2), 24);
        screen.navigate(Nav::Scroll(ScrollBy::Lines(5)), 24);
        let rows = screen.frame(80, 24);

        // Assert
        assert!(
            rows[1].text.starts_with("[job-3] line 13"),
            "{}",
            rows[1].text
        );
        assert!(
            rows[0]
                .text
                .contains("too small for panes  scrolled: 5 lines below "),
            "{}",
            rows[0].text
        );
    }

    #[test]
    fn maps_the_keys_that_move_about_the_screen() {
        fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
            KeyEvent::new(code, modifiers)
        }
        let none = KeyModifiers::NONE;
        let control = KeyModifiers::CONTROL;
        assert_eq!(
            navigation_for(key(KeyCode::Up, none)),
            Some(Nav::Scroll(ScrollBy::Lines(1)))
        );
        assert_eq!(
            navigation_for(key(KeyCode::Char('j'), none)),
            Some(Nav::Scroll(ScrollBy::Lines(-1)))
        );
        assert_eq!(
            navigation_for(key(KeyCode::Char('p'), control)),
            Some(Nav::Scroll(ScrollBy::Lines(1)))
        );
        assert_eq!(
            navigation_for(key(KeyCode::PageDown, none)),
            Some(Nav::Scroll(ScrollBy::Pages(-1)))
        );
        assert_eq!(
            navigation_for(key(KeyCode::Char('G'), KeyModifiers::SHIFT)),
            Some(Nav::Scroll(ScrollBy::Bottom))
        );
        assert_eq!(
            navigation_for(key(KeyCode::Char('7'), none)),
            Some(Nav::Select(7))
        );
        assert_eq!(
            navigation_for(key(KeyCode::BackTab, KeyModifiers::SHIFT)),
            Some(Nav::Previous)
        );
        // Ctrl-C and the keys that leave the screen are not navigation.
        assert_eq!(navigation_for(key(KeyCode::Char('c'), control)), None);
        assert_eq!(navigation_for(key(KeyCode::Enter, none)), None);
        assert_eq!(navigation_for(key(KeyCode::Char('q'), none)), None);
        assert_eq!(navigation_for(key(KeyCode::Char('j'), control)), None);
    }

    #[test]
    fn a_tiny_terminal_still_gets_a_footer_and_never_overflows() {
        // Arrange
        let screen = planned(4, &["a", "b", "c", "d"]);

        // Act
        let rows = screen.frame(10, 2);

        // Assert
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert!(display_width(&row.text) <= 10, "{}", row.text);
        }
    }

    #[test]
    fn a_pane_sets_its_next_job_off_from_the_last_one() {
        // Arrange
        let mut screen = planned(1, &["first", "second"]);
        screen.apply(Event::Started {
            worker: 0,
            index: 0,
        });
        screen.apply(Event::Line {
            worker: 0,
            index: 0,
            line: "first output".into(),
        });
        screen.apply(Event::Finished {
            worker: 0,
            index: 0,
            result: succeeded(),
        });
        screen.apply(Event::Started {
            worker: 0,
            index: 1,
        });
        screen.apply(Event::Line {
            worker: 0,
            index: 1,
            line: "second output".into(),
        });

        // Act
        let rows = screen.frame(60, 9);

        // Assert: the earlier output stays, and a rule names the job whose
        // lines follow.
        assert!(rows[0].text.contains(" second "), "{}", rows[0].text);
        assert!(rows[1].text.starts_with("first output"));
        assert!(
            rows[2].text.starts_with("\u{2500}\u{2500} ok"),
            "{}",
            rows[2].text
        );
        assert_eq!(rows[3].text.trim(), "");
        assert!(
            rows[4].text.starts_with("\u{2500}\u{2500} second"),
            "{}",
            rows[4].text
        );
        assert_eq!(rows[4].color, Some(Color::DarkGrey));
        assert!(rows[5].text.starts_with("second output"));
    }

    /// Two jobs on two workers, both started, with the given `--close-after`.
    fn two_running(close_after: Vec<Option<Duration>>) -> Screen {
        let mut screen = planned(2, &["server", "open"]).with_close_after(close_after);
        for index in 0..2 {
            screen.apply(Event::Started {
                worker: index,
                index,
            });
        }
        screen
    }

    fn finish(screen: &mut Screen, index: usize, result: JobResult) {
        screen.apply(Event::Finished {
            worker: index,
            index,
            result,
        });
    }

    #[test]
    fn closes_the_pane_of_a_job_that_succeeded_once_its_time_is_up() {
        // Arrange
        let mut screen = two_running(vec![None, Some(Duration::from_secs(3))]);
        let start = screen.now;
        finish(&mut screen, 1, succeeded());

        // Act: the countdown in the title, and the frame after the time is up.
        let counting = screen.frame(60, 21);
        let ticked = screen.advance(start + Duration::from_millis(1500));
        let still_counting = screen.frame(60, 21);
        let closed = screen.advance(start + Duration::from_secs(3));
        let rows = screen.frame(60, 21);

        // Assert: two panes of ten rows while it counts down, then the one
        // left open has all twenty.
        assert!(
            counting[10].text.contains(" open  done  closes in 3s "),
            "{}",
            counting[10].text
        );
        assert!(ticked);
        assert!(
            still_counting[10]
                .text
                .contains(" open  done  closes in 2s "),
            "{}",
            still_counting[10].text
        );
        assert!(closed);
        let texts = texts(&rows);
        assert!(texts[0].contains(" server "), "{}", texts[0]);
        assert!(texts[1..20].iter().all(|text| !text.contains(" open ")));
        assert!(!screen.is_finished_without_panes(21));
    }

    #[test]
    fn redraws_only_when_the_countdown_shows_another_second() {
        // Arrange
        let mut screen = two_running(vec![None, Some(Duration::from_secs(3))]);
        let start = screen.now;
        finish(&mut screen, 1, succeeded());

        // Act / Assert: 3s left shows as 3s until the first second is gone.
        assert!(!screen.advance(start + Duration::from_millis(500)));
        assert!(!screen.advance(start + Duration::from_millis(999)));
        assert!(screen.advance(start + Duration::from_millis(1001)));
    }

    #[test]
    fn zero_closes_the_pane_as_the_job_ends() {
        // Arrange
        let mut screen = two_running(vec![Some(Duration::ZERO); 2]);

        // Act
        finish(&mut screen, 1, succeeded());
        let rows = screen.frame(60, 21);

        // Assert
        let texts = texts(&rows);
        assert!(texts[0].contains(" server "), "{}", texts[0]);
        assert!(texts[1..20].iter().all(|text| !text.contains(" open ")));
    }

    #[test]
    fn a_delay_past_the_clock_keeps_the_pane_instead_of_panicking() {
        // Arrange
        let mut screen = two_running(vec![None, Some(Duration::MAX)]);

        // Act
        finish(&mut screen, 1, succeeded());
        let rows = screen.frame(60, 21);

        // Assert
        assert!(rows[10].text.contains(" open  done "), "{}", rows[10].text);
        assert!(!rows[10].text.contains("closes"), "{}", rows[10].text);
    }

    #[cfg(unix)]
    #[test]
    fn keeps_the_pane_of_a_job_that_failed() {
        // Arrange
        let mut screen = two_running(vec![Some(Duration::ZERO); 2]);
        let start = screen.now;

        // Act
        finish(&mut screen, 1, failed(1));
        screen.advance(start + Duration::from_secs(60));
        let rows = screen.frame(60, 21);

        // Assert
        assert!(
            rows[10].text.contains(" open  FAILED "),
            "{}",
            rows[10].text
        );
        assert!(!rows[10].text.contains("closes"), "{}", rows[10].text);
    }

    #[test]
    fn an_empty_value_keeps_the_pane() {
        // Arrange: `--close-after ',0'`, so only the second job closes.
        let mut screen = two_running(vec![None, Some(Duration::ZERO)]);
        let start = screen.now;

        // Act
        finish(&mut screen, 0, succeeded());
        finish(&mut screen, 1, succeeded());
        screen.advance(start + Duration::from_secs(60));
        let rows = screen.frame(60, 21);

        // Assert
        assert!(rows[0].text.contains(" server  done "), "{}", rows[0].text);
        assert!(!rows[0].text.contains("closes"), "{}", rows[0].text);
        assert!(
            texts(&rows)[1..20]
                .iter()
                .all(|text| !text.contains(" open "))
        );
    }

    #[test]
    fn leaves_without_a_key_once_every_pane_has_closed() {
        // Arrange
        let mut screen = two_running(vec![Some(Duration::from_secs(1)); 2]);
        let start = screen.now;
        finish(&mut screen, 0, succeeded());
        finish(&mut screen, 1, succeeded());
        screen.all_done = true;

        // Act
        let while_counting = screen.is_finished_without_panes(21);
        screen.advance(start + Duration::from_secs(1));
        let closed = screen.is_finished_without_panes(21);
        let rows = screen.frame(60, 21);

        // Assert: nothing is left to draw but the footer.
        assert!(!while_counting);
        assert!(closed);
        assert!(rows[..20].iter().all(|row| row.text.trim().is_empty()));
    }

    #[test]
    fn a_closed_pane_comes_back_when_its_worker_takes_another_job() {
        // Arrange: one worker, two jobs, the first closing at once.
        let mut screen =
            planned(1, &["first", "second"]).with_close_after(vec![Some(Duration::ZERO); 2]);
        screen.apply(Event::Started {
            worker: 0,
            index: 0,
        });
        finish(&mut screen, 0, succeeded());
        let closed = screen.frame(60, 9);

        // Act
        screen.apply(Event::Started {
            worker: 0,
            index: 1,
        });
        let rows = screen.frame(60, 9);

        // Assert
        assert!(closed[0].text.trim().is_empty(), "{}", closed[0].text);
        assert!(rows[0].text.contains(" second "), "{}", rows[0].text);
    }

    #[test]
    fn the_merged_log_ignores_closing_and_waits_for_a_key() {
        // Arrange: ten workers on 24 rows give the merged log.
        let names: Vec<String> = (0..10).map(|i| format!("job-{i}")).collect();
        let mut screen = Screen::new(names).with_close_after(vec![Some(Duration::ZERO); 10]);
        screen.apply(Event::Planned { workers: 10 });
        for index in 0..10 {
            screen.apply(Event::Started {
                worker: index,
                index,
            });
            finish(&mut screen, index, succeeded());
        }
        screen.all_done = true;

        // Act
        let rows = screen.frame(80, 24);

        // Assert: the log is still there, and the screen waits as ever; on a
        // tall enough terminal the closed panes would have let it go.
        assert!(
            rows[0].text.contains("too small for panes"),
            "{}",
            rows[0].text
        );
        assert!(
            rows[1].text.starts_with("[job-0] ── ok"),
            "{}",
            rows[1].text
        );
        assert!(!screen.is_finished_without_panes(24));
        assert!(screen.is_finished_without_panes(41));
    }

    #[test]
    fn formats_durations_for_the_scale_they_are_on() {
        assert_eq!(format_duration(Duration::from_millis(340)), "0.3s");
        assert_eq!(format_duration(Duration::from_millis(12_540)), "12.5s");
        assert_eq!(format_duration(Duration::from_secs(185)), "3m05s");
        assert_eq!(format_duration(Duration::from_secs(3_720)), "1h02m");
    }
}

//! The full screen view of a run: one pane per worker, each showing the tail
//! of what its commands wrote, redrawn as the lines arrive.

use crate::{
    process::{self, Isolation},
    runner::{self, Event, Job, JobResult, Outcome},
    terminal::TerminalSession,
    text::{display_width, fit_to_width, plain_text, truncate_to_width},
};
use anyhow::Result;
use crossterm::{
    cursor::MoveTo,
    event::{self, Event as TerminalEvent, KeyCode, KeyEventKind, KeyModifiers},
    queue,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal::{self, Clear, ClearType},
};
use std::{
    collections::VecDeque,
    io::Write,
    sync::mpsc::{self, TryRecvError},
    thread,
    time::{Duration, Instant},
};

/// Lines kept per pane. A pane shows its tail, and a command that writes more
/// than this has said what it needed to say long before the end.
const PANE_HISTORY: usize = 500;
const MERGED_HISTORY: usize = 2_000;
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

struct Pane {
    /// The job the worker holds or last held.
    job: Option<usize>,
    state: PaneState,
    lines: VecDeque<String>,
}

pub struct Screen {
    names: Vec<String>,
    panes: Vec<Pane>,
    merged: VecDeque<String>,
    finished: Vec<JobResult>,
    abort: Abort,
    all_done: bool,
}

/// One row of a frame, as text and the colour it is drawn in.
#[derive(Debug, PartialEq, Eq)]
pub struct Row {
    text: String,
    color: Option<Color>,
}

impl Screen {
    pub fn new(names: Vec<String>) -> Self {
        Self {
            names,
            panes: Vec::new(),
            merged: VecDeque::new(),
            finished: Vec::new(),
            abort: Abort::None,
            all_done: false,
        }
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
                        lines: VecDeque::new(),
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
                if !pane.lines.is_empty() {
                    push_line(&mut pane.lines, PANE_HISTORY, String::new());
                    push_line(&mut pane.lines, PANE_HISTORY, separator);
                }
                pane.job = Some(index);
                pane.state = PaneState::Running;
            }
            Event::Line {
                worker,
                index,
                line,
            } => {
                let line = plain_text(&line);
                let merged = format!("[{}] {line}", self.name(index));
                push_line(&mut self.merged, MERGED_HISTORY, merged);
                let Some(pane) = self.panes.get_mut(worker) else {
                    return;
                };
                push_line(&mut pane.lines, PANE_HISTORY, line);
            }
            Event::Finished {
                worker,
                index,
                result,
            } => {
                let closing = closing_line(&result);
                let merged = format!("[{}] {closing}", self.name(index));
                push_line(&mut self.merged, MERGED_HISTORY, merged);
                if let Some(pane) = self.panes.get_mut(worker) {
                    pane.state = if result.succeeded() {
                        PaneState::Succeeded
                    } else {
                        PaneState::Failed
                    };
                    push_line(&mut pane.lines, PANE_HISTORY, closing);
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
        let pane_rows = body_rows.checked_div(self.panes.len()).unwrap_or(0);
        let log_rows = pane_rows.saturating_sub(1);
        if log_rows >= MIN_LOG_ROWS {
            for pane in &self.panes {
                rows.push(title_row(&self.pane_title(pane), width, pane_color(pane)));
                rows.extend(tail_rows(&pane.lines, log_rows, width));
            }
        } else if body_rows > 0 {
            let title = if self.panes.is_empty() {
                "parun  starting".to_owned()
            } else {
                format!(
                    "parun  {} workers, one log: the terminal is too small for panes",
                    self.panes.len()
                )
            };
            rows.push(title_row(&title, width, Color::DarkCyan));
            rows.extend(tail_rows(&self.merged, body_rows - 1, width));
        }
        while rows.len() < body_rows {
            rows.push(Row {
                text: " ".repeat(width),
                color: None,
            });
        }
        rows.push(Row {
            text: truncate_to_width(&self.footer(), width.saturating_sub(1)),
            color: Some(Color::DarkCyan),
        });
        rows
    }

    fn pane_title(&self, pane: &Pane) -> String {
        let Some(index) = pane.job else {
            return "waiting".to_owned();
        };
        let name = self.name(index);
        match pane.state {
            PaneState::Waiting | PaneState::Running => name.to_owned(),
            PaneState::Succeeded => format!("{name}  done"),
            PaneState::Failed => format!("{name}  FAILED"),
        }
    }

    fn footer(&self) -> String {
        let done = self.finished.len();
        let total = self.names.len();
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
            return format!("Done: {succeeded} ok, {failed} failed{aborted}    press any key");
        }
        match self.abort {
            Abort::None => format!("{done}/{total} done    Ctrl-C abort"),
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

fn pane_color(pane: &Pane) -> Color {
    match pane.state {
        PaneState::Waiting | PaneState::Running => Color::DarkCyan,
        PaneState::Succeeded => Color::Green,
        PaneState::Failed => Color::Red,
    }
}

fn push_line(lines: &mut VecDeque<String>, capacity: usize, line: String) {
    if lines.len() == capacity {
        lines.pop_front();
    }
    lines.push_back(line);
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
fn title_row(title: &str, width: usize, color: Color) -> Row {
    let label = truncate_to_width(&format!(" {title} "), width.saturating_sub(2));
    let rule = "\u{2500}".repeat(width.saturating_sub(display_width(&label) + 2));
    Row {
        text: fit_to_width(&format!("\u{2500}\u{2500}{label}{rule}"), width),
        color: Some(color),
    }
}

/// The last `count` lines, each cut to the width rather than wrapped: a
/// wrapped line would take a row from the lines above it, and the tail of a
/// log is read for what happened last, not for every column of it.
fn tail_rows(lines: &VecDeque<String>, count: usize, width: usize) -> Vec<Row> {
    let skip = lines.len().saturating_sub(count);
    let mut rows: Vec<Row> = lines
        .iter()
        .skip(skip)
        .map(|line| Row {
            text: fit_to_width(line, width),
            color: line_color(line),
        })
        .collect();
    while rows.len() < count {
        rows.push(Row {
            text: " ".repeat(width),
            color: None,
        });
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
        queue!(stdout, Print(&row.text), ResetColor)?;
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
pub fn run(jobs: Vec<Job>, concurrency: usize) -> Result<ScreenOutcome> {
    let mut terminal = TerminalSession::enter()?;
    let mut screen = Screen::new(jobs.iter().map(|job| job.name.clone()).collect());
    // Bounded like the channel inside `run_jobs`, and for the same reason.
    let (tx, rx) = mpsc::sync_channel(runner::EVENT_QUEUE);
    let worker = thread::spawn(move || {
        runner::run_jobs(&jobs, concurrency, Isolation::OwnGroup, |event| {
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
/// and a key has been pressed.
fn watch(
    terminal: &mut TerminalSession,
    screen: &mut Screen,
    rx: &mpsc::Receiver<Event>,
) -> Result<()> {
    let mut dirty = true;
    let mut last_frame = Instant::now() - FRAME_INTERVAL;
    loop {
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
            draw(
                &mut terminal.stdout,
                screen,
                usize::from(width),
                usize::from(height),
            )?;
            dirty = false;
            last_frame = Instant::now();
        }
        if !event::poll(FRAME_INTERVAL)? {
            continue;
        }
        let key = match event::read()? {
            TerminalEvent::Key(key) => key,
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
        let running = screen.footer();
        screen.all_done = true;

        // Act
        let done = screen.footer();

        // Assert: the fourth job never started, as after an abort.
        assert_eq!(running, "3/4 done    Ctrl-C abort");
        assert_eq!(done, "Done: 2 ok, 1 failed, 1 not started    press any key");
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

    #[test]
    fn formats_durations_for_the_scale_they_are_on() {
        assert_eq!(format_duration(Duration::from_millis(340)), "0.3s");
        assert_eq!(format_duration(Duration::from_millis(12_540)), "12.5s");
        assert_eq!(format_duration(Duration::from_secs(185)), "3m05s");
        assert_eq!(format_duration(Duration::from_secs(3_720)), "1h02m");
    }
}

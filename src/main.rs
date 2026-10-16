//! parun: runs shell commands in parallel, each in its own pane.

mod process;
mod runner;
mod screen;
mod terminal;
mod text;

use anyhow::{Context, Result, bail};
use clap::Parser;
use runner::{Event, Job, JobResult, Outcome};
use std::{
    io::{self, IsTerminal, Read, Write},
    path::PathBuf,
    time::Duration,
};
use text::{display_width, pad_to_width};

/// Lines of a failed command's output printed after the screen has gone: the
/// panes were the only place it was shown, and the reason for the failure is
/// usually at the end.
const FAILURE_TAIL_LINES: usize = 20;

#[derive(Parser, Debug)]
#[command(
    name = "parun",
    version,
    about = "Runs shell commands in parallel, each in its own pane",
    long_about = "Runs shell commands in parallel. On a terminal, each running command gets a \
                  pane showing the tail of its output; when the output is piped, the lines of \
                  every command are printed as they arrive, each with the command's name in \
                  front. A summary follows, and the exit code is 1 if any command failed.\n\n\
                  Each command is a single argument, run with `sh -c`. Ctrl-C asks the running \
                  commands to stop; a second Ctrl-C kills them.",
    after_help = "Examples:\n  \
                  parun 'cargo build' 'npm test' 'ruff check .'\n  \
                  parun -n build,test 'cargo build' 'npm test'\n  \
                  parun -j 2 -f commands.txt\n  \
                  parun --close-after ',3' 'npm run dev' 'open http://localhost:5173/'\n  \
                  printf 'make -C a\\nmake -C b\\n' | parun"
)]
struct Args {
    /// The commands to run, one shell command line each
    #[arg(value_name = "COMMAND")]
    commands: Vec<String>,

    /// Read the commands from a file, one per line; `-` reads standard input.
    /// Blank lines and lines starting with # are skipped
    #[arg(short, long, value_name = "FILE", conflicts_with = "commands")]
    file: Option<PathBuf>,

    /// Names for the commands, in order, comma separated. A command without a
    /// name is called by its command line
    #[arg(short, long, value_name = "NAME", value_delimiter = ',')]
    names: Vec<String>,

    /// How many commands run at a time [default: all of them]
    #[arg(short, long, value_name = "N")]
    jobs: Option<usize>,

    /// Run the commands in this directory instead of the current one
    #[arg(short = 'C', long, value_name = "DIR")]
    directory: Option<PathBuf>,

    /// Close the pane of a command that succeeded this many seconds after it
    /// ends, so the other panes get its rows. Per command, in order, comma
    /// separated like --names; a single value applies to every command, an
    /// empty one keeps that command's pane, and 0 closes it at once. A failed
    /// command's pane stays. Has no effect without panes
    #[arg(
        long,
        value_name = "SECONDS",
        value_delimiter = ',',
        value_parser = parse_close_after
    )]
    close_after: Vec<CloseAfter>,

    /// Print one merged log, as when the output is piped, even on a terminal
    #[arg(long)]
    plain: bool,
}

/// One element of `--close-after`: how long a succeeded command's pane stays,
/// or `None` for an empty element, which keeps it.
#[derive(Debug, Clone, Copy, PartialEq)]
struct CloseAfter(Option<Duration>);

fn parse_close_after(value: &str) -> Result<CloseAfter, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(CloseAfter(None));
    }
    let seconds: f64 = value
        .parse()
        .map_err(|_| format!("`{value}` is not a number of seconds"))?;
    Duration::try_from_secs_f64(seconds)
        .map(|delay| CloseAfter(Some(delay)))
        .map_err(|_| format!("`{value}` is not a number of seconds from 0 up"))
}

fn main() {
    let args = Args::parse();
    if let Err(error) = run(args) {
        terminal::wait_for_signal_exit();
        // An error on the way out of the screen may leave commands running in
        // groups of their own; the stop comes before the message, so that a
        // closed stderr cannot stand between the error and the stop.
        process::stop_running_commands(Duration::from_secs(2));
        let _ = writeln!(io::stderr(), "parun: {error:#}");
        std::process::exit(2);
    }
}

fn run(args: Args) -> Result<()> {
    let jobs = collect_jobs(&args)?;
    let concurrency = match args.jobs {
        Some(0) => bail!("--jobs must be at least 1"),
        Some(jobs) => jobs,
        None => jobs.len(),
    };
    terminal::stop_commands_on_hangup()?;

    let count = jobs.len();
    let close_after = close_after_per_command(&args.close_after, count)?;
    let interactive = !args.plain && io::stdin().is_terminal() && io::stdout().is_terminal();
    let (results, aborted) = if interactive {
        let outcome = screen::run(jobs.clone(), concurrency, close_after)?;
        print_header(count, concurrency.min(count));
        (outcome.results, outcome.aborted)
    } else {
        let results = runner::run_jobs(&jobs, concurrency, |event| match event {
            Event::Planned { workers } => {
                print_header(count, workers);
                print_line("");
            }
            Event::Line { index, line, .. } => {
                print_line(&format!("[{}] {line}", jobs[index].name));
            }
            Event::Finished { index, result, .. } => {
                print_line(&format!(
                    "[{}] \u{2500}\u{2500} {}",
                    jobs[index].name,
                    closing(&result)
                ));
            }
            Event::Started { .. } => {}
        });
        // The run is over; what is still listed is something a command left
        // behind, which would otherwise outlive parun.
        process::stop_leftover_commands(Duration::from_secs(3));
        (results, false)
    };
    // The commands may have ended because a signal is ending parun: the
    // summary is not wanted then, and the signal is what has to end the process.
    terminal::wait_for_signal_exit();

    print_summary(&jobs, &results);
    if interactive {
        print_failure_tails(&jobs, &results);
    }
    let not_started = results.iter().filter(|result| result.is_none()).count();
    if aborted && not_started > 0 {
        print_line(&format!(
            "\naborted: {not_started} command{} never started",
            if not_started == 1 { "" } else { "s" }
        ));
    } else if aborted {
        print_line("\naborted");
    }
    let failed = results
        .iter()
        .any(|result| !result.as_ref().is_some_and(JobResult::succeeded));
    if failed || aborted {
        std::process::exit(1);
    }
    Ok(())
}

/// The commands from the arguments, the file, or standard input, paired with
/// their names and the working directory.
fn collect_jobs(args: &Args) -> Result<Vec<Job>> {
    let commands = if let Some(file) = &args.file {
        read_command_lines(file)?
    } else if !args.commands.is_empty() {
        args.commands.clone()
    } else if !io::stdin().is_terminal() {
        read_command_lines(&PathBuf::from("-"))?
    } else {
        bail!("no commands given; see --help");
    };
    if commands.is_empty() {
        bail!("no commands to run");
    }
    if args.names.len() > commands.len() {
        bail!(
            "{} names were given for {} commands",
            args.names.len(),
            commands.len()
        );
    }
    let directory = match &args.directory {
        None => None,
        Some(directory) => {
            if !directory.is_dir() {
                bail!("{} is not a directory", directory.display());
            }
            Some(directory.clone())
        }
    };
    Ok(commands
        .into_iter()
        .enumerate()
        .map(|(index, command)| {
            let name = args
                .names
                .get(index)
                .filter(|name| !name.trim().is_empty())
                .cloned()
                .unwrap_or_else(|| command.clone());
            Job {
                name,
                command,
                directory: directory.clone(),
            }
        })
        .collect())
}

/// The `--close-after` delay of each of `count` commands. A single value
/// stands for every command; otherwise the values go to the commands in order,
/// like the names, and a command past the last value keeps its pane.
fn close_after_per_command(values: &[CloseAfter], count: usize) -> Result<Vec<Option<Duration>>> {
    if let [CloseAfter(delay)] = values {
        return Ok(vec![*delay; count]);
    }
    if values.len() > count {
        bail!(
            "{} --close-after values were given for {} commands",
            values.len(),
            count
        );
    }
    let mut delays: Vec<Option<Duration>> = values.iter().map(|value| value.0).collect();
    delays.resize(count, None);
    Ok(delays)
}

/// One command per line. Blank lines and comments are skipped, so a file can
/// be annotated and a command can be disabled without being deleted.
fn read_command_lines(path: &PathBuf) -> Result<Vec<String>> {
    let mut text = String::new();
    if path.as_os_str() == "-" {
        io::stdin()
            .read_to_string(&mut text)
            .context("failed to read the commands from standard input")?;
    } else {
        text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
    }
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect())
}

/// Writes one line of the plain output. A reader that has gone away, `head`
/// say, closes the pipe, and going on would only leave the commands running
/// for nobody, so they are stopped and parun ends; `println!` would panic
/// instead, and the panic would wait for every worker before it got anywhere.
fn print_line(line: &str) {
    let mut stdout = io::stdout().lock();
    if writeln!(stdout, "{line}")
        .and_then(|()| stdout.flush())
        .is_err()
    {
        process::stop_running_commands(Duration::from_secs(1));
        std::process::exit(1);
    }
}

fn print_header(count: usize, workers: usize) {
    print_line(&format!(
        "parun: {count} command{}, {workers} at a time",
        if count == 1 { "" } else { "s" }
    ));
}

fn closing(result: &JobResult) -> String {
    match &result.outcome {
        Outcome::NotRun(message) => format!("not run: {message}"),
        outcome => format!(
            "{} ({})",
            outcome.label(),
            screen::format_duration(result.elapsed)
        ),
    }
}

fn print_summary(jobs: &[Job], results: &[Option<JobResult>]) {
    print_line("\nSummary");
    let name_width = jobs
        .iter()
        .map(|job| display_width(&job.name))
        .max()
        .unwrap_or(0);
    let rows: Vec<(String, String, String)> = results
        .iter()
        .map(|result| match result {
            None => ("not started".to_owned(), "-".to_owned(), String::new()),
            Some(result) => {
                let note = match &result.outcome {
                    Outcome::NotRun(message) => format!("  ({message})"),
                    _ => String::new(),
                };
                (
                    result.outcome.label(),
                    screen::format_duration(result.elapsed),
                    note,
                )
            }
        })
        .collect();
    // Wide enough for the widest label present, `terminated by signal` say,
    // rather than a fixed width that the odd one would push out of line.
    let status_width = rows
        .iter()
        .map(|(status, ..)| status.len())
        .max()
        .unwrap_or(0);
    for (job, (status, elapsed, note)) in jobs.iter().zip(rows) {
        let mut line = format!(
            "  {status:<status_width$} {elapsed:>7}  {}",
            pad_to_width(&job.name, name_width)
        );
        if job.name != job.command {
            line.push_str(&format!("  {}", job.command));
        }
        line.push_str(&note);
        print_line(line.trim_end());
    }
}

/// The tail of each failed command's output, printed once the panes that
/// showed it are gone.
fn print_failure_tails(jobs: &[Job], results: &[Option<JobResult>]) {
    for (job, result) in jobs.iter().zip(results) {
        let Some(result) = result else {
            continue;
        };
        if !matches!(result.outcome, Outcome::Failed(_)) || result.output.is_empty() {
            continue;
        }
        let lines: Vec<&str> = result.output.lines().collect();
        let skip = lines.len().saturating_sub(FAILURE_TAIL_LINES);
        let shown = lines.len() - skip;
        print_line(&format!(
            "\n\u{2500}\u{2500} {}: last {shown} line{} of output",
            job.name,
            if shown == 1 { "" } else { "s" }
        ));
        for line in &lines[skip..] {
            print_line(line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close_after(arguments: &[&str]) -> Result<Vec<Option<Duration>>> {
        let args = Args::try_parse_from(arguments)?;
        let count = args.commands.len();
        close_after_per_command(&args.close_after, count)
    }

    const fn seconds(seconds: u64) -> Option<Duration> {
        Some(Duration::from_secs(seconds))
    }

    #[test]
    fn close_after_goes_to_the_commands_in_order() {
        // Act
        let delays = close_after(&["parun", "--close-after", ",,3", "a", "b", "c"]).unwrap();

        // Assert: the empty elements keep their panes.
        assert_eq!(delays, vec![None, None, seconds(3)]);
    }

    #[test]
    fn a_single_close_after_applies_to_every_command() {
        // Act
        let delays = close_after(&["parun", "--close-after", "5", "a", "b", "c"]).unwrap();

        // Assert
        assert_eq!(delays, vec![seconds(5); 3]);
    }

    #[test]
    fn close_after_takes_zero_and_fractions_and_leaves_the_rest_open() {
        // Act
        let delays = close_after(&["parun", "--close-after", "0,1.5", "a", "b", "c"]).unwrap();

        // Assert
        assert_eq!(
            delays,
            vec![
                Some(Duration::ZERO),
                Some(Duration::from_millis(1500)),
                None
            ]
        );
    }

    #[test]
    fn no_close_after_keeps_every_pane() {
        assert_eq!(close_after(&["parun", "a", "b"]).unwrap(), vec![None, None]);
    }

    #[test]
    fn more_close_after_values_than_commands_is_an_error() {
        // Act
        let error = close_after(&["parun", "--close-after", "1,2,3", "a", "b"]).unwrap_err();

        // Assert
        assert_eq!(
            error.to_string(),
            "3 --close-after values were given for 2 commands"
        );
    }

    #[test]
    fn close_after_must_be_a_number_of_seconds() {
        for value in ["soon", "-1", "nan", "1s"] {
            let argument = format!("--close-after={value}");
            let parsed = Args::try_parse_from(["parun", argument.as_str(), "a"]);
            let error = parsed.unwrap_err();
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::ValueValidation,
                "{value}"
            );
        }
    }
}

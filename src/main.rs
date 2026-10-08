//! parun: runs shell commands in parallel, each in its own pane.

mod process;
mod runner;
mod screen;
mod terminal;
mod text;

use anyhow::{Context, Result, bail};
use clap::Parser;
use process::Isolation;
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

    /// Print one merged log, as when the output is piped, even on a terminal
    #[arg(long)]
    plain: bool,
}

fn main() {
    let args = Args::parse();
    if let Err(error) = run(args) {
        eprintln!("parun: {error:#}");
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
    let interactive = !args.plain && io::stdin().is_terminal() && io::stdout().is_terminal();
    let (results, aborted) = if interactive {
        let outcome = screen::run(jobs.clone(), concurrency)?;
        print_header(count, concurrency.min(count));
        (outcome.results, outcome.aborted)
    } else {
        let results =
            runner::run_jobs(
                &jobs,
                concurrency,
                Isolation::SharedGroup,
                |event| match event {
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
                },
            );
        // The commands of this path shared this process's group and are out of
        // reach here, as is anything they left behind.
        process::stop_leftover_commands(Duration::from_secs(3));
        (results, false)
    };

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
/// for nobody; `println!` would panic instead, and the panic would wait for
/// every worker before it got anywhere.
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

//! Running the commands `concurrency` at a time and reporting what they do
//! as events on the calling thread.

use crate::process::{self, lock_ignoring_poison};
use std::{
    path::PathBuf,
    process::ExitStatus,
    sync::{Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

/// One command to run.
#[derive(Debug, Clone)]
pub struct Job {
    /// What the command is called on screen and in the summary.
    pub name: String,
    /// The command line, as the shell gets it.
    pub command: String,
    pub directory: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub enum Outcome {
    Succeeded,
    Failed(ExitStatus),
    /// The command could not be started, or was not allowed to start because
    /// the run had been aborted.
    NotRun(String),
}

impl Outcome {
    /// `ok`, `exit 1`, `terminated by signal`, or `not run`.
    pub fn label(&self) -> String {
        match self {
            Outcome::Succeeded => "ok".into(),
            Outcome::Failed(status) => process::exit_label(*status),
            Outcome::NotRun(_) => "not run".into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct JobResult {
    pub outcome: Outcome,
    /// The last lines the command wrote, both streams together.
    pub output: String,
    pub elapsed: Duration,
}

impl JobResult {
    pub fn succeeded(&self) -> bool {
        matches!(self.outcome, Outcome::Succeeded)
    }
}

/// Events a run may have waiting for its caller before the workers wait.
pub const EVENT_QUEUE: usize = 4_096;

/// What a run reports while it goes. `worker` numbers the slot that took the
/// job, from zero, so a display can keep one place per slot; `index` is the
/// job's position in the list the run was given.
#[derive(Debug, Clone)]
pub enum Event {
    /// Sent once, first, with the number of slots this run uses.
    Planned { workers: usize },
    /// A slot took a job.
    Started { worker: usize, index: usize },
    /// One line of output from the job the slot holds.
    Line {
        worker: usize,
        index: usize,
        line: String,
    },
    /// The slot is done with the job.
    Finished {
        worker: usize,
        index: usize,
        result: JobResult,
    },
}

/// Runs one job to its end, handing each line it writes to `log`.
pub fn run_job<F>(job: &Job, mut log: F) -> JobResult
where
    F: FnMut(String),
{
    let started = Instant::now();
    let mut command = process::shell_command(&job.command);
    if let Some(directory) = &job.directory {
        command.current_dir(directory);
    }
    match process::run_streaming(command, &mut |line| log(line)) {
        Ok(output) => JobResult {
            outcome: if output.status.success() {
                Outcome::Succeeded
            } else {
                Outcome::Failed(output.status)
            },
            output: output.text,
            elapsed: started.elapsed(),
        },
        Err(error) => JobResult {
            outcome: Outcome::NotRun(format!("{error:#}")),
            output: String::new(),
            elapsed: started.elapsed(),
        },
    }
}

/// Runs the jobs `concurrency` at a time and calls `on_event` on the calling
/// thread as the workers report, so a display needs no locking of its own. The
/// results come back in the order of `jobs`; a run abandoned part way leaves
/// `None` for the jobs that were never started.
pub fn run_jobs<F>(jobs: &[Job], concurrency: usize, mut on_event: F) -> Vec<Option<JobResult>>
where
    F: FnMut(Event),
{
    let workers = jobs.len().min(concurrency.max(1));
    on_event(Event::Planned { workers });
    let (job_tx, job_rx) = mpsc::channel::<(usize, &Job)>();
    // Bounded, so that workers wait for a slow display rather than pile lines
    // up in memory; the display is drained on this thread below, for as long
    // as any worker runs.
    let (event_tx, event_rx) = mpsc::sync_channel(EVENT_QUEUE);
    for job in jobs.iter().enumerate() {
        let _ = job_tx.send(job);
    }
    drop(job_tx);
    let job_rx = Mutex::new(job_rx);

    let mut results: Vec<Option<JobResult>> = vec![None; jobs.len()];
    thread::scope(|scope| {
        for worker in 0..workers {
            let event_tx = event_tx.clone();
            let job_rx = &job_rx;
            scope.spawn(move || {
                loop {
                    if process::abandoned() {
                        return;
                    }
                    let Ok((index, job)) = lock_ignoring_poison(job_rx).recv() else {
                        return;
                    };
                    if event_tx.send(Event::Started { worker, index }).is_err() {
                        return;
                    }
                    let result = run_job(job, |line| {
                        let _ = event_tx.send(Event::Line {
                            worker,
                            index,
                            line,
                        });
                    });
                    if event_tx
                        .send(Event::Finished {
                            worker,
                            index,
                            result,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            });
        }
        drop(event_tx);
        for event in event_rx {
            if let Event::Finished { index, result, .. } = &event {
                results[*index] = Some(result.clone());
            }
            on_event(event);
        }
    });
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(name: &str, command: &str) -> Job {
        Job {
            name: name.into(),
            command: command.into(),
            directory: None,
        }
    }

    #[cfg(unix)]
    #[test]
    fn runs_the_jobs_in_slots_and_reports_each_in_order() {
        // Arrange: three jobs on two workers, so one slot takes two jobs.
        let jobs = vec![
            job("a", "sleep 0.3; echo a-done"),
            job("b", "echo b-done; exit 2"),
            job("c", "echo c-done"),
        ];
        let mut events = Vec::new();

        // Act
        let results = run_jobs(&jobs, 2, |event| events.push(event));

        // Assert: every job finished with its own status and output.
        assert_eq!(results.len(), 3);
        let results: Vec<JobResult> = results.into_iter().map(Option::unwrap).collect();
        assert!(results[0].succeeded());
        assert_eq!(results[0].output, "a-done");
        assert!(matches!(results[1].outcome, Outcome::Failed(status) if status.code() == Some(2)));
        assert_eq!(results[1].outcome.label(), "exit 2");
        assert!(results[2].succeeded());

        // The plan comes first, and each job is started before any of its lines.
        assert!(matches!(events[0], Event::Planned { workers: 2 }));
        let started = |wanted: usize| {
            events
                .iter()
                .position(|event| matches!(event, Event::Started { index, .. } if *index == wanted))
        };
        let first_line = |wanted: usize| {
            events
                .iter()
                .position(|event| matches!(event, Event::Line { index, .. } if *index == wanted))
        };
        for index in 0..3 {
            assert!(started(index) < first_line(index), "{events:?}");
        }
        // Workers are numbered from zero, and only two slots were used.
        assert!(events.iter().all(|event| match event {
            Event::Started { worker, .. }
            | Event::Line { worker, .. }
            | Event::Finished { worker, .. } => *worker < 2,
            Event::Planned { .. } => true,
        }));
    }

    #[test]
    fn a_command_that_cannot_start_is_reported_rather_than_fatal() {
        // Arrange: a working directory that does not exist.
        let mut broken = job("x", "echo never");
        broken.directory = Some(PathBuf::from("/nonexistent/parun/dir"));

        // Act
        let result = run_job(&broken, |_| {});

        // Assert
        assert!(matches!(result.outcome, Outcome::NotRun(_)));
        assert_eq!(result.outcome.label(), "not run");
    }
}

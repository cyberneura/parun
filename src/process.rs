//! Running one command and keeping hold of it: its output streamed line by
//! line, its process group on a list, so that a stop reaches it and whatever
//! it started.

use anyhow::{Context, Result, anyhow};
use std::{
    collections::VecDeque,
    io::{BufRead, BufReader, Read},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, RecvTimeoutError, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

/// Process ids of the running commands. Every command gets a process group of
/// its own, so that a stop can take it together with the helpers it started,
/// and that group is listed here so that a stop can find it: nothing else
/// signals a group of its own, and the thread that waits for it dies with this
/// process. A group stays listed while any process in it lives, so a process a
/// command left behind is on it after the command itself has ended.
static RUNNING_COMMANDS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Set while a run is being abandoned, so that the workers stop taking jobs.
/// Killing only what is running would not be enough with more commands than
/// workers: a worker whose command was killed would go on to start the next.
///
/// It is never cleared. Every abort here is followed by the end of the process,
/// and clearing it at the start of a run would let a worker thread that
/// happened to start late undo an abort issued a moment before.
static ABANDONED: AtomicBool = AtomicBool::new(false);

/// Process groups whose command has ended while something in the group lives
/// on: a helper, or a process the command started in the background. They are
/// kept apart from the running commands so that the end of a run can stop them
/// without touching a command another run may have started in the meantime.
static LEFTOVER_GROUPS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Commands being started right now, spawned but not yet on
/// `RUNNING_COMMANDS`. A stop that found the list empty would otherwise return,
/// and the process end, while such a command was about to be registered.
static SPAWNING: AtomicUsize = AtomicUsize::new(0);

/// Lines of a command's output kept for the result, beyond what was handed out
/// as it ran. A command has no timeout and can write without bound.
pub const RETAINED_LINES: usize = 2_000;
/// How long the pipes are read on after the command has ended. A helper the
/// command left behind can hold them open, so the drain has a bound.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const REAP_TIMEOUT: Duration = Duration::from_secs(5);
/// How often a running command is looked at for its exit.
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// How long a stop allows for a command that was starting as the stop came in.
const SPAWN_GRACE: Duration = Duration::from_secs(1);
/// Lines a pipe reader may have waiting for the runner. Past that the reader
/// waits, the pipe fills, and the command waits on its write, as it would on a
/// slow terminal; memory does not grow with the speed of the command.
const PIPE_QUEUE: usize = 1_024;
/// The most of one line that is kept in memory. Output without a newline, a
/// progress bar that only ever redraws itself or a dump of binary data, would
/// otherwise be held whole until the command ends.
const MAX_LINE_BYTES: usize = 64 * 1024;

/// Whether a run has been abandoned: no further command may start.
pub fn abandoned() -> bool {
    ABANDONED.load(Ordering::SeqCst)
}

/// Stops a run: no further command is started, and the commands running in a
/// group of their own are killed. A command sharing this process's group is
/// not touched, because the terminal that put it there delivers Ctrl-C to it
/// directly.
pub fn terminate_running_commands() {
    ABANDONED.store(true, Ordering::SeqCst);
    signal_listed(Signal::Kill, &RUNNING_COMMANDS);
    signal_listed(Signal::Kill, &LEFTOVER_GROUPS);
}

/// Asks a run to stop: no further command is started, and the commands running
/// in a group of their own get the signal Ctrl-C would have given them. Unlike
/// `terminate_running_commands`, this leaves them time to clean up. The
/// commands stay on the list, so a `terminate_running_commands` afterwards
/// still reaches one that ignored the request.
///
/// Groups that have emptied since they were listed are dropped first, here and
/// in `terminate_running_commands`: once a group is gone its id can be given to
/// an unrelated process, and that process must not get the signal.
pub fn interrupt_running_commands() {
    ABANDONED.store(true, Ordering::SeqCst);
    signal_listed(Signal::Interrupt, &RUNNING_COMMANDS);
    signal_listed(Signal::Interrupt, &LEFTOVER_GROUPS);
}

/// Sends the signal to every group on the list that still has a member, and
/// drops the ones that have emptied. The list is not held while the signals
/// go out: on Windows a signal is a `taskkill` to wait for, and a thread
/// registering or releasing a command must not wait behind it.
fn signal_listed(signal: Signal, list: &Mutex<Vec<u32>>) {
    let alive = {
        let mut listed = lock_ignoring_poison(list);
        listed.retain(|pid| listed_group_alive(*pid));
        listed.clone()
    };
    signal_process_groups(&alive, signal);
}

/// Stops a run for good, on the way out of the process: asks the running
/// commands to stop, gives them `grace` to do so, and kills whatever is still
/// running after that. A request alone is not enough here, because a command
/// that ignores it would outlive the screen that started it, with nothing left
/// to press Ctrl-C at.
pub fn stop_running_commands(grace: Duration) {
    interrupt_running_commands();
    if wait_for_running_commands(grace) {
        return;
    }
    terminate_running_commands();
    wait_for_running_commands(SPAWN_GRACE);
}

/// Stops what a finished run left behind: a process a command started in the
/// background, still alive in a group that the run is done with. Nothing is
/// abandoned by this. A command that means to leave a process behind has to
/// put it in a session of its own, with `setsid` or the like.
pub fn stop_leftover_commands(grace: Duration) {
    signal_listed(Signal::Interrupt, &LEFTOVER_GROUPS);
    if wait_for_list(&LEFTOVER_GROUPS, grace) {
        return;
    }
    signal_listed(Signal::Kill, &LEFTOVER_GROUPS);
    wait_for_list(&LEFTOVER_GROUPS, SPAWN_GRACE);
}

/// Waits for the list to empty, dropping groups that have ended on their own.
/// Returns whether it emptied within `grace`.
fn wait_for_list(list: &Mutex<Vec<u32>>, grace: Duration) -> bool {
    let deadline = Instant::now() + grace;
    loop {
        let mut listed = lock_ignoring_poison(list);
        listed.retain(|pid| listed_group_alive(*pid));
        let empty = listed.is_empty();
        drop(listed);
        if empty {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Waits for both lists to empty and for no command to be part way through
/// starting. Returns whether that came about within `grace`.
fn wait_for_running_commands(grace: Duration) -> bool {
    let deadline = Instant::now() + grace;
    loop {
        let running = {
            let mut running = lock_ignoring_poison(&RUNNING_COMMANDS);
            running.retain(|pid| listed_group_alive(*pid));
            running.len()
        };
        let leftover = {
            let mut leftover = lock_ignoring_poison(&LEFTOVER_GROUPS);
            leftover.retain(|pid| listed_group_alive(*pid));
            leftover.len()
        };
        if running + leftover == 0 && SPAWNING.load(Ordering::SeqCst) == 0 {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

#[derive(Clone, Copy)]
enum Signal {
    Interrupt,
    Kill,
}

/// Poisoning carries no meaning here: a panic under one of these locks leaves
/// nothing half-written for the next holder to find.
pub fn lock_ignoring_poison<T>(lock: &Mutex<T>) -> MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// What a pipe reader sends back: the lines as they come, then one `Eof`.
enum PipeMessage {
    Line(String),
    Eof,
}

/// Reads a pipe line by line on its own thread. Each pipe needs a thread of its
/// own: draining them one after the other lets the command fill the second pipe
/// and block, while this side is still waiting for the first to reach EOF.
fn stream(pipe: impl Read + Send + 'static, tx: SyncSender<PipeMessage>) -> Result<()> {
    // `thread::Builder` reports a thread that could not be started rather than
    // panicking, which would unwind past the child and leave it running.
    thread::Builder::new()
        .spawn(move || {
            let mut reader = BufReader::new(pipe);
            let mut buffer = Vec::new();
            loop {
                buffer.clear();
                match read_line_bounded(&mut reader, &mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                if tx.send(PipeMessage::Line(output_line(&buffer))).is_err() {
                    return;
                }
            }
            let _ = tx.send(PipeMessage::Eof);
        })
        .context("failed to start a thread to read command output")?;
    Ok(())
}

/// Reads up to and including the next newline, or `MAX_LINE_BYTES` if no
/// newline comes first; the rest of such a line arrives as further lines.
/// Returns the number of bytes read, zero at the end of the pipe.
fn read_line_bounded(reader: &mut impl BufRead, buffer: &mut Vec<u8>) -> std::io::Result<usize> {
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(buffer.len());
        }
        let room = MAX_LINE_BYTES - buffer.len();
        let (taken, done) = match available.iter().position(|byte| *byte == b'\n') {
            Some(index) if index < room => (index + 1, true),
            _ => (available.len().min(room), false),
        };
        buffer.extend_from_slice(&available[..taken]);
        reader.consume(taken);
        if done || buffer.len() == MAX_LINE_BYTES {
            return Ok(buffer.len());
        }
    }
}

/// Turns raw bytes of one line into text. A progress bar redraws itself with
/// carriage returns rather than new lines, so only what came after the last one
/// is kept: that is what a terminal would have left on screen.
fn output_line(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let text = text.trim_end_matches(['\n', '\r']);
    text.rsplit('\r').next().unwrap_or_default().to_owned()
}

/// Puts the command in a process group of its own, so that killing the group
/// takes the helpers it started with it.
#[cfg(unix)]
fn own_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
fn own_process_group(_command: &mut Command) {}

/// Kills the command by group, so the helpers it started go with it.
#[cfg(unix)]
fn kill_command(child: &mut Child) {
    signal_process_group(child.id(), Signal::Kill);
    let _ = child.kill();
}

/// Signals the groups directly rather than through a `kill` binary: there is no
/// binary to go missing or hang, and nothing waits on a subprocess while the
/// running list is held.
#[cfg(unix)]
fn signal_process_groups(pids: &[u32], signal: Signal) {
    for pid in pids {
        signal_process_group(*pid, signal);
    }
}

#[cfg(unix)]
fn signal_process_group(pid: u32, signal: Signal) {
    let signal = match signal {
        Signal::Interrupt => libc::SIGINT,
        Signal::Kill => libc::SIGKILL,
    };
    let Ok(group) = libc::pid_t::try_from(pid) else {
        return;
    };
    // SAFETY: `killpg` takes a group id and a signal number and touches no
    // memory of ours; a group that no longer exists is reported as an error,
    // which is ignored here.
    unsafe {
        libc::killpg(group, signal);
    }
}

/// Windows has no process group to signal, so the command's process tree is
/// taken down by `taskkill` instead, forcibly in either case: there is no
/// interruption a console-less process would receive.
#[cfg(windows)]
fn kill_command(child: &mut Child) {
    signal_process_group(child.id(), Signal::Kill);
    let _ = child.kill();
}

/// The `taskkill`s are started together and waited for together, so that the
/// pids are all acted on in the same moment and a stop takes one bound in all
/// rather than one per pid.
#[cfg(windows)]
fn signal_process_groups(pids: &[u32], _signal: Signal) {
    let mut children: Vec<Child> = pids
        .iter()
        .filter_map(|pid| {
            Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok()
        })
        .collect();
    let deadline = Instant::now() + REAP_TIMEOUT;
    while Instant::now() < deadline {
        children.retain_mut(|child| matches!(child.try_wait(), Ok(None)));
        if children.is_empty() {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    for mut child in children {
        let _ = child.kill();
    }
}

#[cfg(windows)]
fn signal_process_group(pid: u32, signal: Signal) {
    signal_process_groups(&[pid], signal);
}

/// Collects the child, giving up rather than waiting without a bound. A child
/// that outlasts the wait is left for the operating system to reap when this
/// process exits.
fn reap(child: &mut Child) {
    let deadline = Instant::now() + REAP_TIMEOUT;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            _ => return,
        }
    }
}

/// Starts a command in a process group of its own and puts it on the running
/// list, so that a stop reaches it. The whole of that, from before the spawn to
/// after the registration, counts as spawning, and a stop waits for spawning to
/// finish: an abort that comes in during it is answered here, by killing the
/// command as soon as it is registered, before the count drops.
fn spawn_tracked(mut command: Command) -> Result<Child> {
    SPAWNING.fetch_add(1, Ordering::SeqCst);
    let spawned = spawn_tracked_counted(&mut command);
    SPAWNING.fetch_sub(1, Ordering::SeqCst);
    spawned
}

fn spawn_tracked_counted(command: &mut Command) -> Result<Child> {
    if abandoned() {
        return Err(anyhow!("aborted before the command started"));
    }
    own_process_group(command);
    let mut child = command.spawn().context("failed to run command")?;
    lock_ignoring_poison(&RUNNING_COMMANDS).push(child.id());
    if abandoned() {
        kill_command(&mut child);
        reap(&mut child);
        forget_running(&child);
        return Err(anyhow!("aborted before the command started"));
    }
    Ok(child)
}

/// What a command left behind: its exit status and the last lines it wrote to
/// either stream, in the order the lines arrived.
#[derive(Debug)]
pub struct CommandOutput {
    pub status: ExitStatus,
    pub text: String,
}

/// Runs the command, handing each line it writes to `on_line` as it arrives.
/// The command runs in a process group of its own and is listed while it runs,
/// so a stop kills it together with the helpers it started.
pub fn run_streaming(
    mut command: Command,
    on_line: &mut dyn FnMut(String),
) -> Result<CommandOutput> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = spawn_tracked(command)?;
    let (tx, rx) = mpsc::sync_channel(PIPE_QUEUE);
    let readers = stream(
        child.stdout.take().expect("stdout is piped above"),
        tx.clone(),
    )
    .and_then(|()| stream(child.stderr.take().expect("stderr is piped above"), tx));
    if let Err(error) = readers {
        kill_command(&mut child);
        reap(&mut child);
        forget_running(&child);
        return Err(error);
    }

    let mut lines = VecDeque::new();
    let mut open_pipes = 2;
    let mut exited: Option<(ExitStatus, Instant)> = None;
    loop {
        // A command that closed both its pipes and ran on, one that started a
        // daemon or redirected its output, has nothing more to say: the channel
        // is disconnected and would return at once, so the wait is slept
        // instead of spun.
        if open_pipes == 0 {
            thread::sleep(POLL_INTERVAL);
        }
        let message = if open_pipes == 0 {
            Err(RecvTimeoutError::Disconnected)
        } else {
            rx.recv_timeout(POLL_INTERVAL)
        };
        match message {
            Ok(PipeMessage::Line(line)) => {
                on_line(line.clone());
                if lines.len() == RETAINED_LINES {
                    lines.pop_front();
                }
                lines.push_back(line);
            }
            Ok(PipeMessage::Eof) => open_pipes -= 1,
            // A reader thread that died takes its sender with it; there is
            // nothing more to wait for from that pipe.
            Err(RecvTimeoutError::Disconnected) => open_pipes = 0,
            Err(RecvTimeoutError::Timeout) => {}
        }
        if exited.is_none() {
            match child.try_wait() {
                Ok(Some(status)) => {
                    forget_running(&child);
                    exited = Some((status, Instant::now() + DRAIN_TIMEOUT));
                }
                Ok(None) => {}
                Err(error) => {
                    kill_command(&mut child);
                    reap(&mut child);
                    forget_running(&child);
                    return Err(error).context("failed to wait for command");
                }
            }
        }
        let Some((status, drain_deadline)) = exited else {
            continue;
        };
        if open_pipes == 0 || Instant::now() >= drain_deadline {
            return Ok(CommandOutput {
                status,
                text: Vec::from(lines).join("\n"),
            });
        }
    }
}

/// Takes the command off the running list once it has ended. A process group
/// that still has members, a helper the command started or a background
/// process it left behind, moves to the leftover list, so that a stop still
/// reaches them; it is let go when it has emptied. A pid cannot be reused
/// while a group with that id exists, so a kept entry names the right
/// processes.
#[cfg(unix)]
fn forget_running(child: &Child) {
    let pid = child.id();
    // Onto the leftover list before it leaves the running list, so that at no
    // moment is a live group on neither: a stop that looked between the two
    // would find nothing left to wait for and return with the group alive.
    {
        let mut leftover = lock_ignoring_poison(&LEFTOVER_GROUPS);
        leftover.retain(|other| group_alive(*other));
        if group_alive(pid) {
            leftover.push(pid);
        }
    }
    let mut running = lock_ignoring_poison(&RUNNING_COMMANDS);
    if let Some(index) = running.iter().position(|other| *other == pid) {
        running.swap_remove(index);
    }
}

/// Whether a listed group is still worth signalling. On Windows there is no
/// group to ask, so an entry is trusted for as long as its command runs and
/// dropped the moment the command has ended.
#[cfg(unix)]
fn listed_group_alive(pid: u32) -> bool {
    group_alive(pid)
}

#[cfg(windows)]
fn listed_group_alive(_pid: u32) -> bool {
    true
}

/// Whether any process in the group still exists. Signal zero checks without
/// delivering anything; a group of another user's processes, which answers
/// with a permission error, is taken as alive.
#[cfg(unix)]
fn group_alive(pid: u32) -> bool {
    let Ok(group) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: as in `signal_process_group`; signal zero delivers nothing.
    let answer = unsafe { libc::killpg(group, 0) };
    answer == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn forget_running(child: &Child) {
    let pid = child.id();
    let mut running = lock_ignoring_poison(&RUNNING_COMMANDS);
    if let Some(index) = running.iter().position(|other| *other == pid) {
        running.swap_remove(index);
    }
}

/// The command line handed to the shell. `sh -c` on Unix, as `make`, `xargs`
/// and `find -exec` do, so that pipes, globs and `&&` mean what they mean at a
/// prompt.
#[cfg(unix)]
pub fn shell_command(command: &str) -> Command {
    let mut shell = Command::new("sh");
    shell.args(["-c", command]);
    shell
}

#[cfg(windows)]
pub fn shell_command(command: &str) -> Command {
    let mut shell = Command::new("cmd");
    shell.args(["/C", command]);
    shell
}

/// `exit 1`, or `terminated by signal` for a command that was killed.
pub fn exit_label(status: ExitStatus) -> String {
    status.code().map_or_else(
        || "terminated by signal".into(),
        |code| format!("exit {code}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ABANDONED` and `RUNNING_COMMANDS` are shared by the whole process, and
    /// every command reads the one and joins the other. A test that sets the
    /// flag or stops the running commands takes this exclusively; a test that
    /// merely runs commands takes it shared, so those still run together.
    static ABANDON_FLAG: std::sync::RwLock<()> = std::sync::RwLock::new(());

    fn shared() -> std::sync::RwLockReadGuard<'static, ()> {
        ABANDON_FLAG
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn exclusive() -> std::sync::RwLockWriteGuard<'static, ()> {
        let guard = ABANDON_FLAG
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        ABANDONED.store(false, Ordering::SeqCst);
        guard
    }

    #[test]
    fn splits_a_line_that_never_ends_instead_of_holding_it_whole() {
        // Arrange: three times the bound, with no newline anywhere.
        let data = vec![b'x'; MAX_LINE_BYTES * 3];
        let mut reader = BufReader::new(data.as_slice());
        let mut pieces = Vec::new();

        // Act
        loop {
            let mut buffer = Vec::new();
            let read = read_line_bounded(&mut reader, &mut buffer).unwrap();
            if read == 0 {
                break;
            }
            pieces.push(buffer.len());
        }

        // Assert
        assert_eq!(pieces, vec![MAX_LINE_BYTES; 3]);

        // And an ordinary line comes back whole, newline included.
        let mut reader = BufReader::new(&b"one\ntwo"[..]);
        let mut buffer = Vec::new();
        assert_eq!(read_line_bounded(&mut reader, &mut buffer).unwrap(), 4);
        assert_eq!(buffer, b"one\n");
        buffer.clear();
        assert_eq!(read_line_bounded(&mut reader, &mut buffer).unwrap(), 3);
        assert_eq!(buffer, b"two");
    }

    #[test]
    fn keeps_the_last_frame_of_a_line_redrawn_with_carriage_returns() {
        assert_eq!(output_line(b"10%\r50%\r100%\n"), "100%");
        assert_eq!(output_line(b"plain\r\n"), "plain");
        assert_eq!(output_line(b"no newline"), "no newline");
    }

    #[cfg(unix)]
    #[test]
    fn hands_out_each_line_as_it_arrives_and_keeps_the_last_progress_frame() {
        // Arrange: a line, a progress bar redrawn with carriage returns, a line
        // on the other stream, and a last line without a newline.
        let _serial = shared();
        let mut seen = Vec::new();

        // Act
        let output = run_streaming(
            shell_command(
                "echo first; printf '10%%\\r50%%\\r100%%\\n'; echo warning >&2; sleep 0.2; printf last",
            ),
            &mut |line| seen.push(line),
        )
        .unwrap();

        // Assert: the two streams are read by two threads, so only the order
        // within one stream is promised.
        assert!(output.status.success());
        let position = |wanted: &str| seen.iter().position(|line| line == wanted);
        assert!(position("first") < position("100%"), "{seen:?}");
        assert!(position("100%") < position("last"), "{seen:?}");
        assert!(!seen.iter().any(|line| line.contains('\r')), "{seen:?}");
        assert!(seen.contains(&"warning".to_owned()), "{seen:?}");
        assert_eq!(seen.last().map(String::as_str), Some("last"));
        assert_eq!(output.text, seen.join("\n"));
    }

    #[cfg(unix)]
    #[test]
    fn collects_output_larger_than_a_pipe_buffer_from_both_streams() {
        // Arrange
        let _serial = shared();
        let (mut stdout, mut stderr) = (0, 0);

        // Act
        let output = run_streaming(
            shell_command("yes stderr | head -c 400000 >&2; yes stdout | head -c 400000"),
            &mut |line| match line.as_str() {
                "stdout" | "stdou" => stdout += 1,
                _ => stderr += 1,
            },
        )
        .unwrap();

        // Assert: `yes` writes one word per line and `head` cuts the last one
        // short, so every line is handed out while the result keeps the tail.
        assert!(output.status.success());
        assert_eq!(stdout, 400_000_usize.div_ceil("stdout\n".len()));
        assert_eq!(stderr, 400_000_usize.div_ceil("stderr\n".len()));
        assert_eq!(output.text.lines().count(), RETAINED_LINES);
    }

    #[cfg(unix)]
    #[test]
    fn reports_the_exit_status_of_a_failing_command() {
        // Exclusive, because the running list is asserted empty at the end
        // and another test's command would be on it.
        let _serial = exclusive();

        let output = run_streaming(shell_command("echo oops >&2; exit 3"), &mut |_| {}).unwrap();

        assert_eq!(output.status.code(), Some(3));
        assert_eq!(exit_label(output.status), "exit 3");
        assert_eq!(output.text, "oops");
        assert!(lock_ignoring_poison(&RUNNING_COMMANDS).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn kills_a_command_that_starts_after_an_abort() {
        // Arrange: the abort came in before this command could be registered,
        // so its signal found nothing of it to hit.
        let _serial = exclusive();
        ABANDONED.store(true, Ordering::SeqCst);
        let mut command = Command::new("sleep");
        command.arg("30");
        let started = Instant::now();

        // Act
        let error =
            run_streaming(command, &mut |_| {}).expect_err("the command is not allowed to run on");
        ABANDONED.store(false, Ordering::SeqCst);

        // Assert
        assert!(error.to_string().contains("aborted"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(lock_ignoring_poison(&RUNNING_COMMANDS).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_stop_waits_for_the_commands_it_asked_to_stop_and_kills_the_rest() {
        // Arrange: one command that stops when asked, one that ignores it.
        let _serial = exclusive();
        let mut polite = Command::new("sleep");
        polite.arg("30");
        let stubborn = shell_command("trap '' INT; sleep 30");
        let outcomes = Mutex::new(Vec::new());
        let started = Instant::now();

        // Act
        thread::scope(|scope| {
            for command in [polite, stubborn] {
                let outcomes = &outcomes;
                scope.spawn(move || {
                    let output = run_streaming(command, &mut |_| {});
                    lock_ignoring_poison(outcomes)
                        .push(output.map(|output| output.status.success()));
                });
            }
            thread::sleep(Duration::from_millis(500));
            stop_running_commands(Duration::from_secs(2));
        });
        // The stop leaves the flag set, as it would on the way out of the
        // process; the next test starts from a clean one.
        ABANDONED.store(false, Ordering::SeqCst);

        // Assert: both ended, neither with success, and the stop did not wait
        // out the 30 seconds.
        let outcomes = lock_ignoring_poison(&outcomes);
        assert_eq!(outcomes.len(), 2);
        assert!(
            outcomes.iter().all(|outcome| matches!(outcome, Ok(false))),
            "{outcomes:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(lock_ignoring_poison(&RUNNING_COMMANDS).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn keeps_a_group_listed_while_a_background_process_of_the_command_lives() {
        // Arrange: the command itself ends at once and leaves a process behind.
        // A helper of an earlier test's command may still be winding down in
        // a group of its own; those are cleared first, so that the one left
        // behind here is the only one on the list.
        let _serial = exclusive();
        stop_leftover_commands(Duration::from_secs(2));

        // Act
        let output =
            run_streaming(shell_command("sleep 30 >/dev/null 2>&1 &"), &mut |_| {}).unwrap();
        let running = lock_ignoring_poison(&RUNNING_COMMANDS).clone();
        let leftover = lock_ignoring_poison(&LEFTOVER_GROUPS).clone();
        stop_leftover_commands(Duration::from_secs(2));
        thread::sleep(Duration::from_millis(200));

        // Assert: the group outlived the command, moved to the leftovers, and
        // the stop of those took it down without abandoning anything.
        assert!(output.status.success());
        assert!(running.is_empty(), "{running:?}");
        assert_eq!(leftover.len(), 1, "{leftover:?}");
        assert!(!group_alive(leftover[0]));
        assert!(lock_ignoring_poison(&LEFTOVER_GROUPS).is_empty());
        assert!(!abandoned());
    }
}

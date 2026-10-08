//! The terminal in raw mode on the alternate screen, and the way out of it
//! when a signal ends the process.

use crate::process;
use anyhow::{Result, bail};
use crossterm::{
    cursor::{Hide, Show},
    execute,
    style::ResetColor,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::{
    io,
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
};

/// Set by the signal thread once it has decided to end the process, so that no
/// screen is entered after the terminal was put back.
static EXITING: AtomicBool = AtomicBool::new(false);

/// Held while the terminal is being put into raw mode or taken out of it on
/// the way out, so that the two cannot interleave: a screen entered just after
/// the signal thread had looked would be left raw when the process dies.
static TERMINAL_GATE: Mutex<()> = Mutex::new(());

fn terminal_gate() -> MutexGuard<'static, ()> {
    TERMINAL_GATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Whether a signal is on its way to end the process. The thread that took it
/// stops the commands and then dies of the signal; nothing else should reach
/// `exit` first, or the shell would see an exit code in place of the signal.
pub fn exiting() -> bool {
    EXITING.load(Ordering::SeqCst)
}

/// Gives the signal thread time to end the process. Called where this thread
/// would otherwise exit on its own while `exiting()` is set.
pub fn wait_for_signal_exit() {
    if exiting() {
        std::thread::sleep(std::time::Duration::from_secs(10));
    }
}

/// Raw mode on the alternate screen, from `enter` until the drop.
pub struct TerminalSession {
    pub stdout: io::Stdout,
}

impl TerminalSession {
    pub fn enter() -> Result<Self> {
        let _gate = terminal_gate();
        if EXITING.load(Ordering::SeqCst) {
            bail!("parun is exiting");
        }
        terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, Hide) {
            let _ = terminal::disable_raw_mode();
            return Err(error.into());
        }
        Ok(Self { stdout })
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(self.stdout, Show, LeaveAlternateScreen, ResetColor);
    }
}

/// How long a stop on the way out waits for the commands to end on their own
/// before killing them.
const STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// Stops the commands when the terminal goes away or parun is told to end.
/// The commands run in process groups of their own, so neither a hangup of the
/// terminal nor a signal sent to parun's pid reaches them, and the default
/// handling would end parun alone and leave them running. The thread stops
/// them, puts the terminal back if a screen is up, and then dies of the signal
/// itself rather than exiting with a code: a shell script decides whether to
/// stop on Ctrl-C by whether its child died of SIGINT, not by the code it
/// returned.
///
/// SIGINT is on the list for a plain run, where a Ctrl-C at the terminal
/// arrives here as a signal and is passed on to the commands this way.
#[cfg(unix)]
pub fn stop_commands_on_hangup() -> Result<()> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    let mut signals = signal_hook::iterator::Signals::new([SIGHUP, SIGINT, SIGTERM])?;
    std::thread::Builder::new().spawn(move || {
        let Some(signal) = signals.forever().next() else {
            return;
        };
        // Set before the stop, so that the main thread, whose commands are
        // about to end, sees it in time and waits for this thread rather than
        // reaching `exit` with a code of its own.
        EXITING.store(true, Ordering::SeqCst);
        process::stop_running_commands(STOP_GRACE);
        // A screen may be up: its session is not dropped on this way out, and
        // the terminal would be left in raw mode on the alternate screen. No
        // new screen may start from here on either, which `EXITING` sees to.
        // The gate is held to the end, so that no screen starts between the
        // look at the terminal and the death of the process.
        let _gate = terminal_gate();
        if terminal::is_raw_mode_enabled().unwrap_or(false) {
            let _ = execute!(io::stdout(), Show, LeaveAlternateScreen, ResetColor);
            let _ = terminal::disable_raw_mode();
        }
        // The call does not return: it raises the signal with its default
        // action restored, and aborts if it cannot.
        let _ = signal_hook::low_level::emulate_default_handler(signal);
        std::process::exit(128 + signal);
    })?;
    Ok(())
}

#[cfg(windows)]
pub fn stop_commands_on_hangup() -> Result<()> {
    Ok(())
}

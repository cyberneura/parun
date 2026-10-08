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
/// The commands under the screen run in process groups of their own, so a
/// hangup of the terminal does not reach them, and the default handling would
/// end parun alone and leave them running. The thread stops them, puts the
/// terminal back if a screen is up, and then dies of the signal itself.
///
/// SIGINT is on the list for a plain run, whose commands share this process's
/// group and get the Ctrl-C themselves: parun has nothing of its own to stop
/// there, but still has to die of the signal rather than exit with a code. A
/// shell script decides whether to stop on Ctrl-C by whether its child died of
/// SIGINT, not by the code it returned.
#[cfg(unix)]
pub fn stop_commands_on_hangup() -> Result<()> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    let mut signals = signal_hook::iterator::Signals::new([SIGHUP, SIGINT, SIGTERM])?;
    std::thread::Builder::new().spawn(move || {
        let Some(signal) = signals.forever().next() else {
            return;
        };
        process::stop_running_commands(STOP_GRACE);
        // A screen may be up: its session is not dropped on this way out, and
        // the terminal would be left in raw mode on the alternate screen. No
        // new screen may start from here on either.
        EXITING.store(true, Ordering::SeqCst);
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

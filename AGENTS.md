# parun development notes

A Rust CLI that runs shell commands in parallel and, on a terminal, shows the tail of the output in a pane per worker.
See README.md for the specification and usage. Only development decisions and caveats are written here.

It takes the `sync` screen (`sync_screen.rs`) and the process management (spawn / signals / cleanup in
`pullkit-core`) from [pullkit](https://github.com/cyberneura/pullkit), strips the git-specific parts, and
turns them into a standalone command. Many of the design commitments are inherited from pullkit.

## Language policy

This is a public repository, so **the README, code comments, UI strings and error messages are all written in English**.
**This AGENTS.md is in English too.** It is the development notes for agents, and developer documentation
in the public repositories is kept in English as well.

## Layout

| Path | Contents |
|---|---|
| `src/main.rs` | CLI arguments, collecting the command list (arguments / `--file` / stdin), plain output, Summary, exit code |
| `src/runner.rs` | `Job` / `Event` / `JobResult`. Runs on worker threads and streams events to the calling thread |
| `src/process.rs` | Launching one command and streaming its lines, tracking process groups, two-stage Ctrl-C, cleanup |
| `src/screen.rs` | The pane screen. Frame assembly (`Screen::frame`) and drawing / key input (`run` / `watch`) |
| `src/terminal.rs` | Entering and leaving raw mode + the alternate screen, cleanup on SIGHUP / SIGTERM / SIGINT |
| `src/text.rs` | Measuring and truncating terminal cell width, stripping escape sequences and control characters |

## Development commands

```bash
cargo run -- 'sleep 1; echo a' 'echo b; exit 1'   # pane screen
cargo run -- --plain 'echo a' 'echo b'              # plain output even on a terminal
cargo test
cargo clippy --all-targets --locked -- -D warnings
cargo fmt --all
```

The demo run / check / release can be launched from `.j-menu.yaml`.

## How to verify

**Always run `cargo build` before E2E.** `cargo test` / `cargo clippy` do not relink `target/debug/parun`,
so if you touch the real binary after a fix without building, you see the old behaviour.

**The `parun` on PATH is the Homebrew cask and is different from your local build.** Verify by giving the
path explicitly, e.g. `./target/debug/parun`.

### Plain output

Connecting stdout to a pipe takes the plain path (`./target/debug/parun ... | cat`). stdin not being a
terminal is also a condition, so when run from Claude Code's Bash tool it is plain without doing anything.

### Pane screen

You can capture the real screen through a pty. Launch it with Python's `pty.fork()`, give it a size with
`TIOCSWINSZ`, strip escape sequences from the output, and read the lines. When "Enter quit" appears, send Enter (navigation keys do not exit). Ctrl-C
arrives as raw-mode key input if you write `\x03`. When verifying interruption, go as far as checking with
`pgrep -fl "^sleep 30"` after exit that no children are left.

## Design commitments

- **Always run commands in their own process group and register them in `RUNNING_COMMANDS`**. pullkit put
  only the plain path in a shared group so the terminal's Ctrl-C reached it directly, but parun is used in
  places where signals arrive only at parun's pid, such as `kill <pid>`, PID 1 in Docker, and
  `parun ... | tee`. So that no children are left there, the signal thread stops them with
  `stop_running_commands` even on the plain path (changed after a review comment).
  On the raw-mode screen Ctrl-C does not become a signal but key input, so the screen sends
  `interrupt_running_commands` (SIGINT), and on the second press sends `terminate_running_commands` (SIGKILL).
  When the screen exits for its own reasons it uses `stop_running_commands` (SIGINT → grace period → SIGKILL).
- **When the terminal disappears (SIGHUP) and on SIGTERM, catch the signal ourselves, run `stop_running_commands`, and then exit**
  (`terminal::stop_commands_on_hangup`). The terminal hangup does not reach children in their own groups, and if
  only parun dies first with the default action, everything is left behind. After stopping, kill ourselves with
  the same signal rather than calling `exit()` (`emulate_default_handler`). bash decides whether to stop a script
  based on whether the child died from SIGINT, so going back to `exit(130)` would make scripts that call the plain
  path not stop on Ctrl-C. On the plain path Ctrl-C from the terminal reaches only parun (children are in other
  groups), and this thread forwards it to the children.
- **Keep interruption (`stop_running_commands`) and cleanup (`stop_leftover_commands`) separate**. The former
  sets `ABANDONED`, so that process can no longer start a run. Stopping processes a command left behind after
  the run has finished is the job of the latter.
- **Once `ABANDONED` is set, never reset it**. Interruption is assumed to always end with the process exiting;
  resetting it at the start of a run would let a worker thread that started late cancel the preceding interruption.
- **Keep a process group in `RUNNING_COMMANDS` while it still has members, and remove ones that have become
  empty before sending a signal**. If the group still has members after the command finishes, move it to
  `LEFTOVER_GROUPS`. The id of an empty group can be reused by another process, so check with
  `killpg(pgid, 0)` before sending. Send signals via libc rather than the `kill` binary, and release the
  list's Mutex before sending.
- **Count from spawn to registration in `SPAWNING`, and have stop wait until it reaches 0.** An interruption
  that arrives before registration is answered by the spawner killing the process itself right after registering.
- **The pipe line length (`MAX_LINE_BYTES`) and the channels (`PIPE_QUEUE` / `EVENT_QUEUE`) are bounded**. So
  that memory does not grow with output that has no newlines or with a UI whose drawing cannot keep up, when
  they overflow the child process's write is made to wait.
- **Stream output line by line**. `run_jobs` runs on as many threads as there are workers, and events are
  passed to `on_event` on the calling thread (so drawing needs only one thread). Emitting output in bulk
  leaves a pane empty for the whole duration of a long build.
- **Strip escape sequences and control characters from lines put into a pane** (`text::plain_text`). Colours
  and cursor movement cannot be reproduced inside a pane, and they break the width calculation. Tabs are
  replaced with spaces up to the tab stop. The plain path passes output through as is (the terminal can
  interpret it).
- **When counting columns, use terminal cells, not character counts**. Use `display_width` / `truncate_to_width` /
  `pad_to_width`; do not handle width with `chars().count()` or `{:<20}`. Truncate at grapheme cluster
  boundaries and do not split emoji in the middle.
- **Do not fill only the last line of the frame to the full width**. Some terminals scroll when a character is
  placed in the bottom-right corner.
- **Pane scrollback (CYBERNEURA-DEV-1109)**: each pane and the merged log are a `Log` (a bounded
  `VecDeque` + `top: Option<usize>`); when `top` is `None` it follows the tail. The scroll position is held
  **as a line number from the start**, not as "distance from the end" — holding a distance makes the view
  drift every time a new line arrives while you are reading. When the history overflows and `pop_front` runs,
  decrement `top` by 1 so the same line stays in view. When the view returns to the end, set it back to
  `None` and follow again (`Log::scroll`). The pane that keys act on is `Screen::active`
  (a worker number); if the active pane is closed, it defers to the first open pane (`active_among`).
  The number `[n]` and the `>` marker are shown only when two or more panes are open.
  History is bounded by line count (`PANE_HISTORY` 2,000 / `MERGED_HISTORY` 5,000) and **also by byte count**
  (`PANE_BYTES` 4 MiB / `MERGED_BYTES` 8 MiB; pointed out by Codex). A single line can be up to
  `MAX_LINE_BYTES` = 64 KiB, so with a line count alone one pane could hold 125 MiB. When exceeded, drop the
  oldest lines and keep `top` in step.
  **`EnableMouseCapture` is on while the screen is shown, for the mouse wheel** (`TerminalSession`).
  With it on, drag selection in the terminal requires Shift (stated in the README). The SIGHUP / SIGTERM path
  (`stop_commands_on_hangup`) also emits `DisableMouseCapture` before exiting.
  After all commands have finished, navigation keys (those for which `navigation_for` returns `Some`) do not
  leave the screen; any other key does. That is why it says "Enter quit" rather than "press any key".
- **Launch commands with `sh -c`**. This is the same convention as `make` / `xargs` / `find -exec`; `$SHELL` is
  not used (the syntax changes with fish and the like). stdin is `null`.
- **`print_line` on the plain path stops the children and exits immediately once it can no longer write**. When the
  reader closes first, as with `head`, `println!` panics, and that panic ends up waiting for all workers to finish.

## Release

Changing the version in `Cargo.toml` on `main` and pushing triggers a release (`.github/workflows/release.yml`).
The `plan` job asks the releases API "has a Release for this version been published?", and only on 404 does it
proceed to test → build → release. The decision is based on state, not on a diff, so a failed run continues
from where it left off once you fix the cause and push. The setup is the same as `cyberneura/runandlog` (see the `release-rust-cli` skill).

- Numbering and pushing are done by `scripts/release.sh [patch|minor|major]` (default minor).
- build is macOS arm64 (Developer ID signing + notarytool notarization) and Linux x86_64.
- The signing Secrets (APPLE_*) are distributed from 1Password by adding `cyberneura/parun` to `repos` in
  `~/home-files/sh/github-secret/deploy-github-secret-apple-building.sh` and running it. If they are missing,
  build fails at the first step (it does not silently ship unsigned).
- Publishing to crates.io runs only when both the repository variable `PUBLISH_CRATES=true` and the secret
  `CARGO_REGISTRY_TOKEN` are present.
- Homebrew is `Casks/parun.rb` in `cyberneura/homebrew-tap`. The tap's `scripts/update.py` checks the latest
  release every hour and rewrites version / url / sha256, so do not push to the tap from this repository.
- `test.yml` also runs on PRs (fmt / clippy -D warnings / test). release calls the same definition via
  `workflow_call`. The compiler is pinned by `rust-toolchain.toml`.

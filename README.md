# parun

Runs shell commands in parallel, each in its own pane.

```shell
parun 'cargo build' 'npm test' 'ruff check .'
```

On a terminal, every running command gets a pane showing the tail of what it writes, redrawn as
the lines arrive. When the output is piped, the lines of every command are printed as they come,
each with the command's name in front. Either way a summary follows, and the exit code is 1 if
any command failed.

## Install

macOS on Apple Silicon, through Homebrew:

```shell
brew install --cask cyberneura/tap/parun
```

The binary is signed with a Developer ID and notarized, so the first run needs no workaround.

From crates.io, or from a checkout, on any platform with a Rust toolchain:

```shell
cargo install parun
cargo install --path .
```

The releases page carries built binaries for Apple Silicon and for x86_64 Linux as well. The
Linux one is a glibc build, so a distribution older than the one it was built on may refuse it;
build from source there.

## Usage

```
parun [OPTIONS] [COMMAND]...

  -f, --file <FILE>      Read the commands from a file, one per line; `-` reads standard input
  -n, --names <NAME>     Names for the commands, in order, comma separated
  -j, --jobs <N>         How many commands run at a time [default: all of them]
  -C, --directory <DIR>  Run the commands in this directory instead of the current one
      --plain            Print one merged log, as when the output is piped, even on a terminal
```

Each command is one argument and is handed to `sh -c` (`cmd /C` on Windows), so pipes, globs,
`&&` and redirections mean what they mean at a prompt. A command is called by its command line
unless `--names` gives it a shorter name; the name is what the pane title, the merged log and the
summary show.

```shell
parun -n build,test 'cargo build' 'npm test'
parun -j 2 -f commands.txt
printf 'make -C a\nmake -C b\n' | parun
```

Commands read from a file or from standard input are one per line; blank lines and lines starting
with `#` are skipped, so a list can be annotated and a command can be switched off without being
deleted. With no commands on the command line and standard input not a terminal, the commands are
read from standard input.

## Watching a run

On a terminal, parun takes over the screen with one pane per worker. Each pane shows the command
its worker is on and the tail of what it wrote, line by line as it arrives; colour and cursor
sequences in the output are taken out, since a pane cannot honour them. When a pane would have
fewer than three log rows, because the terminal is short or the commands many, the panes give way
to one merged log with the command's name in front of each line. A pane whose command has ended
names its outcome in the title, green for success and red for failure, and the rule that closes the
command's part of the pane carries the exit status and the time it took.

Ctrl-C asks the running commands to stop, so they can clean up; a second Ctrl-C kills them. Once
every command is done, a key press leaves the screen, and the summary is printed to the terminal,
followed by the last lines of output of each command that failed, since the panes that showed
them are gone.

When standard input or standard output is not a terminal, a pipe or a cron job, or when `--plain`
is given, the lines of every command are printed as they arrive with the command's name in front,
and a line names each command's outcome as it ends. The commands then stay in parun's own process
group, so a Ctrl-C typed at a terminal that parun's output is piped through still reaches them.

Standard input is not passed to the commands; they get an empty one.

## Process handling

Under the screen, anything parun started is stopped when the run ends, including a process a
command left running in the background: it shares the command's process group, and that group is
stopped whether the run was aborted or ran to the end. A command that means to leave a process
behind has to start it in a session of its own, with `setsid` or the like. In the plain output the
commands share parun's own group, and a process a command leaves behind there is left alone, as any
other command's would be.

If the terminal goes away while the screen is up, or parun is told to terminate, the commands are
stopped the same way before parun ends.

On Windows there are no process groups to signal; a stop takes the command's process tree down
with `taskkill` instead, forcibly. Windows is not tested.

## Exit code

| Code | Meaning |
|---|---|
| 0 | Every command succeeded |
| 1 | A command failed, could not be started, or the run was aborted |
| 2 | parun itself could not run: bad arguments, an unreadable file |

## Releasing

A release follows the version in `Cargo.toml` on `main`: change it there and push, and
`.github/workflows/release.yml` builds, signs, notarizes, and publishes it. Nothing else starts
one, and a version that is already released is left alone however often `main` is pushed.

```shell
scripts/release.sh          # minor bump
scripts/release.sh patch
scripts/release.sh major
```

The script bumps `Cargo.toml` and the lockfile together, commits `Release vX.Y.Z`, and pushes
`main`. The workflow runs the tests, builds `parun` for `aarch64-apple-darwin` and
`x86_64-unknown-linux-gnu`, signs and notarizes the macOS binary, and publishes the archives on a
GitHub Release with their checksums in the notes. If a run fails, fix the cause and push: the
version is not released yet, so the next run carries on. Publishing to crates.io is switched on
with the repository variable `PUBLISH_CRATES=true` and the secret `CARGO_REGISTRY_TOKEN`.

The Homebrew cask in [cyberneura/homebrew-tap](https://github.com/cyberneura/homebrew-tap)
(`Casks/parun.rb`) is updated by the tap itself, which looks at the latest release every hour.
This repository never pushes to the tap.

## License

parun is released under the [MIT License](LICENSE).

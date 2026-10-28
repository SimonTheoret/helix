# Embedded terminal

`:terminal` (alias `:term`) opens an interactive terminal over the bottom half
of the editor. It starts `$SHELL` on Unix, `%COMSPEC%` on Windows, or the
platform default shell, in Helix's current working directory.

While the terminal has focus:

| Keys | Action |
| --- | --- |
| `Ctrl-\`, then `Ctrl-n` | Hide the terminal and return to editing |
| `:terminal` from the editor | Resume the same terminal |
| `Ctrl-\`, then `Ctrl-q` | Close the terminal and terminate its shell |
| `Ctrl-\`, then `Ctrl-\` | Send a literal `Ctrl-\` to the shell |
| `Shift-PageUp` / `Shift-PageDown` | Browse scrollback |
| Mouse wheel | Browse scrollback |

Other keys, including Escape and Ctrl-C, go to the child process. The terminal
keeps running while hidden. When the shell exits, its output remains visible
until you close it. Open `:terminal` again after closing to start a new shell.
Terminal keystrokes are excluded from editor macro recordings.

The implementation is Rust: `portable-pty` supplies Unix PTYs and Windows
ConPTY, and `alacritty_terminal` maintains the terminal screen and parses output.
Version 0.25.0 is pinned to support the release's Rust 1.82 toolchain. It supports
ANSI colours (including indexed and RGB colours), text attributes, Unicode and wide cells,
alternate screens, application cursor keys, bracketed paste, terminal queries,
resizing with line reflow, synchronized output, cursor shapes, palette changes,
and 10,000 lines of scrollback. PTY reads, writes, and process waiting
run on separate threads, with bounded queues and editor redraw notifications.
Pastes larger than 1 MiB are rejected; a full input queue reports an error.

This first implementation has one terminal session, shown as an overlay rather
than an editor split. Mouse events browse local scrollback; they are not sent to
applications. Graphics protocols, hyperlinks, and extended keyboard protocols
are not implemented. Terminal clipboard requests are ignored.

## Build and validation

This branch is based on upstream Helix **25.07.1** (the release requested as
25.07.01), commit `a05c151bb6e8e9c65ec390b0ae2afe7a5efd619b`.
It retains the release's Rust 1.82.0 toolchain and crossterm integration.
Upstream PR #14746 is backported for the maintained Go-template grammar and
its matching queries.
With that toolchain in a complete checkout:

```sh
cargo fmt --all
cargo check -p helix-term --locked
cargo test -p helix-term --locked terminal::
cargo test -p helix-term --locked --features integration
cargo build -p helix-term --locked --release
```

The dependency versions are recorded in `Cargo.lock`. CI uses `--locked` and
checks the Rust 1.82 build on Linux and Windows. The included Rust
tests cover input encoding, paste envelopes, fragmented Unicode, ANSI colours,
device and color responses, alternate screens, scrollback, synchronized-output
timeouts, cursor shapes, text attributes, viewport geometry, and an
actual Unix PTY exchanging input, resizing, and exiting. Windows ConPTY needs
validation on Windows.

For a manual smoke test, run `hx`, invoke `:terminal`, run `printf '\033[31mred\033[0m\n'`,
run an interactive program such as `less`, resize the outer terminal, hide the
pane, and reopen it. Check that the same shell and its output remain. Exit the
shell and verify that its final output remains readable before closing the pane.

## Validation

The Rust sources in commit `c8bff2ce850318baf671c43f3accebaf62ee26f0` passed
the [Terminal workflow](https://github.com/SimonTheoret/helix/actions/runs/40763885603)
with Rust 1.82.0 on Linux and Windows: formatting, compilation of all editor
test targets, the terminal test suite, and compilation with the `integration`
feature. The Linux suite includes a live PTY test covering shell input,
resizing, output, and exit. The checks use the committed dependency lockfile.

Automatic grammar builds are disabled for this focused workflow. The
Go-template grammar from PR #14746 was compiled separately. A complete
interactive editor smoke test and interactive Windows ConPTY validation
remain manual checks.

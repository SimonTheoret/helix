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
The 0.25 library series supports the release's Rust 1.82 toolchain. It supports ANSI colours
(including indexed and RGB colours), text attributes, Unicode and wide cells,
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
With that toolchain in a complete checkout:

```sh
cargo fmt --all
cargo check -p helix-term
cargo test -p helix-term terminal::
cargo test -p helix-term --features integration
cargo build -p helix-term --release
```

Cargo must resolve the new dependencies and update `Cargo.lock`; include that
generated lockfile when committing in a complete checkout. The included Rust
tests cover input encoding, paste envelopes, fragmented Unicode, ANSI colours,
device and color responses, alternate screens, scrollback, synchronized-output
timeouts, cursor shapes, text attributes, viewport geometry, and an
actual Unix PTY exchanging input, resizing, and exiting. Windows ConPTY needs
validation on Windows.

For a manual smoke test, run `hx`, invoke `:terminal`, run `printf '\033[31mred\033[0m\n'`,
run an interactive program such as `less`, resize the outer terminal, hide the
pane, and reopen it. Check that the same shell and its output remain. Exit the
shell and verify that its final output remains readable before closing the pane.

## Validation status of this patch

This patch was prepared from upstream source fetched through the GitHub
connector. The execution environment had no Rust toolchain and its network
proxy was unreachable, so compilation, formatting, dependency resolution, and
test execution could not be performed. `git diff --check` passed, and
`git apply --check` passed against source files verified to match upstream Git
blob hashes. Compilation and test execution still need to be completed in a
full checkout with the release's Rust toolchain.

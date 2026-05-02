# Flake

Flake is a terminal-first text editor with a VS Code-style layout for Unix systems. It is built in Rust with a TUI frontend, file explorer, command palette, inline suggestion support, and a Codex subscription chat pane.

## Features

- terminal UI with editor, explorer, status bar, and AI panel
- file open, save, tabs, split panes, and new-buffer flow
- undo/redo, selection, clipboard, and current-line editing shortcuts
- inline prediction hook for Codex-backed suggestions
- ChatGPT Plus/Pro subscription login via OAuth
- change the Codex model at runtime
- workspace-aware explorer with git status markers
- built-in task runner output for build/test/run
- XDG-based config and session state

## Project Layout

- `src/main.rs` starts the app and handles CLI args
- `src/app.rs` owns the TUI, keybindings, and commands
- `src/editor.rs` implements the text buffer
- `src/ai.rs` handles Codex auth and request logic
- `src/config.rs` loads config and session state
- `src/fs.rs` provides directory explorer entries

## Build and Run

```bash
cargo run -- path/to/file
```

## Install

Install the release binary into `~/.local/bin/flake` with:

```bash
./scripts/install.sh
```

Make sure `~/.local/bin` is on your `PATH`:

```bash
export PATH="$HOME/.local/bin:$PATH"
```

Useful commands:

- `cargo check` for a fast compile check
- `cargo test` to run unit tests
- `cargo fmt` to format the codebase
- `cargo run -- --help` to show keybindings and commands

## Configuration

Flake reads config from:

- `~/.config/flake/config.toml`
- fallback to `XDG_CONFIG_HOME/flake/config.toml`

It stores session state under:

- `~/.local/state/flake/state.toml`
- fallback to `XDG_STATE_HOME/flake/state.toml`

Codex auth is stored in `~/.local/state/flake/auth.json` and refreshed automatically after login.

## How to Test

Run the automated checks first:

```bash
cargo test
cargo check
```

Then verify the interactive app:

```bash
cargo run -- --help
cargo run -- .
```

Manual checks to perform in the TUI:

- open a file from the explorer
- edit text and save with `Ctrl-S`
- open the command palette with `Ctrl-P` or `Ctrl-Shift-P`
- open help with `F1`
- create a new buffer with `Ctrl-N`
- reload the current file with `Ctrl-R`
- undo and redo with `Ctrl-Z` and `Ctrl-Y`
- close the current buffer with `Ctrl-W`
- cycle buffers with `Ctrl-Tab` and `Ctrl-Shift-Tab`
- toggle split view with `Ctrl-\`
- reopen the last closed buffer with `Ctrl-Shift-T`
- search in the current file with `Ctrl-F`
- jump through matches with `Ctrl-G` and `Ctrl-Shift-G`
- go to a line with `Ctrl-L`
- delete the current line with `Ctrl-D`
- duplicate the current line with `Ctrl-Shift-D`
- copy, cut, and paste with `Ctrl-C`, `Ctrl-X`, and `Ctrl-V`
- select text with Shift+Arrow keys or by dragging with the mouse
- toggle hidden files with `Ctrl-H`
- confirm quit with `Ctrl-Q` when the buffer is modified
- watch the bottom 3-line terminal strip for recent actions and AI output
- toggle the explorer with `Ctrl-B`
- focus the explorer with `Ctrl-E`
- open the AI chat with `F5`
- run build/test/run tasks with `build`, `test`, `run`, or `rerun`
- change the model with `model gpt-5.4-mini` in the command palette or `/model gpt-5.4-mini` in chat
- log in with `login` in the command palette or `/login` in chat
- close the current buffer with `close`, `/close`, `Ctrl-W`, or `/close`
- close other buffers with `close other` or `/close other`
- duplicate the current line with `duplicate line`
- reopen closed buffers with `reopen closed` or `/reopen closed`
- inspect the workspace with `/pwd`, `/ls`, `/tree`, and `/cat`
- manage tabs from the palette with `tab next` and `tab prev`
- use the mouse wheel to scroll panes and click explorer entries

For Codex features, run `login` once and complete the browser OAuth flow.

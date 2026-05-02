# Repository Guidelines

## Project Structure & Module Organization
`flake` is a Rust TUI editor. Source lives in `src/`:
- `src/main.rs` bootstraps the app and CLI args.
- `src/app.rs` owns the TUI, key handling, panes, and command palette.
- `src/editor.rs` contains the text buffer and editing primitives.
- `src/ai.rs` handles Codex OAuth auth and request logic.
- `src/config.rs` loads config and XDG state.
- `src/fs.rs` reads directory entries for the explorer.

Unit tests currently live next to the code in the same files. Add new tests beside the module they cover.

## Build, Test, and Development Commands
- `cargo run -- [path]` starts Flake in the terminal and optionally opens a file.
- `cargo check` type-checks quickly without producing a release binary.
- `cargo test` runs the Rust unit tests.
- `cargo fmt` formats the tree using `rustfmt`.

## Coding Style & Naming Conventions
Use Rust 2024 edition defaults and keep formatting `rustfmt`-clean. Prefer small, explicit modules and direct names:
- Types: `CamelCase` like `CodexClient`
- Functions and fields: `snake_case` like `load_state`
- Commands and config paths should stay Unix-friendly and XDG-based

Keep comments short and only for non-obvious logic. Avoid broad refactors that mix editor, config, and AI changes in one patch.

## Testing Guidelines
Use standard Rust unit tests with `#[cfg(test)]` and `#[test]`. Name tests after behavior, for example `backspace_merges_previous_line`. Add tests for:
- buffer editing edge cases
- config/state round-trips
- command parsing
- AI response parsing where practical

Run `cargo test` before handing off changes.

## Commit & Pull Request Guidelines
This repository currently has no established Git history or commit convention. Use short imperative commit messages if you create commits, for example `add explorer focus handling`.

For pull requests, include:
- a brief summary of behavior changes
- commands used to verify the change
- screenshots or terminal recordings for UI work
- notes on any Codex or Unix compatibility assumptions

## Agent Coordination
If another agent is already editing a slice, do not overwrite it. Keep changes isolated to your assigned files and adapt to existing work instead of reverting it.

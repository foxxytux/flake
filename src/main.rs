mod ai;
mod app;
mod config;
mod editor;
mod fs;

use anyhow::Result;
use std::path::PathBuf;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut initial_path = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_help();
                return Ok(());
            }
            "-v" | "--version" => {
                println!("flake 0.1.0");
                return Ok(());
            }
            path => {
                initial_path = Some(PathBuf::from(path));
            }
        }
    }

    let config = config::load()?;
    let app = app::App::new(config, initial_path)?;
    app.run()
}

fn print_help() {
    println!("flake 0.1.0");
    println!("usage: flake [path]");
    println!("controls:");
    println!("  ctrl-s   save");
    println!("  ctrl-q   quit");
    println!("  ctrl-p   command palette");
    println!("  ctrl-shift-p   command palette");
    println!("  ctrl-b   toggle explorer");
    println!("  ctrl-h   toggle hidden files");
    println!("  ctrl-e   focus explorer");
    println!("  ctrl-a   toggle codex pane");
    println!("  ctrl-n   new buffer");
    println!("  ctrl-r   reload current file");
    println!("  ctrl-z   undo");
    println!("  ctrl-y   redo");
    println!("  ctrl-f   search current file");
    println!("  ctrl-g   next search match");
    println!("  ctrl-shift-g   previous search match");
    println!("  ctrl-l   go to line");
    println!("  ctrl-d   delete current line");
    println!("  ctrl-shift-d   duplicate current line");
    println!("  ctrl-w   close current buffer");
    println!("  ctrl-c   copy selection or line");
    println!("  ctrl-x   cut selection or line");
    println!("  ctrl-v   paste clipboard");
    println!("  ctrl-tab   next buffer");
    println!("  ctrl-shift-tab   previous buffer");
    println!("  ctrl-\\   toggle split view");
    println!("  ctrl-shift-t   reopen closed buffer");
    println!("  shift-arrows   select text");
    println!("  f1       help");
    println!("  tab      accept inline suggestion");
    println!("  f5       open codex chat");
    println!("commands:");
    println!("  open PATH");
    println!("  new PATH");
    println!("  reload");
    println!("  build");
    println!("  test");
    println!("  run");
    println!("  rerun");
    println!("  login");
    println!("  model NAME");
    println!("  help");
    println!("  close");
    println!("  duplicate line");
    println!("  split");
    println!("  close other");
    println!("  reopen closed");
    println!("  tab next");
    println!("  tab prev");
    println!("chat commands:");
    println!("  /login");
    println!("  /open PATH");
    println!("  /save");
    println!("  /close");
    println!("  /undo");
    println!("  /redo");
    println!("  /split");
    println!("  /tab next");
    println!("  /tab prev");
    println!("  /close other");
    println!("  /reopen closed");
    println!("  /model NAME");
    println!("  /pwd");
    println!("  /ls [PATH]");
    println!("  /tree [PATH]");
    println!("  /cat PATH");
}

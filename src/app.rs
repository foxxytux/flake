use crate::ai::{self, CodexClient, ConversationState, PredictionContext};
use crate::config::{self, AppState, Config};
use crate::editor::TextBuffer;
use crate::fs::{self, Entry};
use anyhow::{Context, Result};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::{
    Frame,
    layout::Position,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
};
use std::cell::Cell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

const PREDICT_DEBOUNCE: Duration = Duration::from_millis(250);
const TICK_RATE: Duration = Duration::from_millis(16);
const AI_SCROLL_STEP: usize = 3;

pub struct App {
    pub config: Config,
    pub explorer_dir: PathBuf,
    pub explorer_entries: Vec<Entry>,
    pub explorer_git_status: HashMap<PathBuf, GitStatus>,
    pub explorer_selected: usize,
    pub explorer_scroll: usize,
    pub editor: TextBuffer,
    pub tabs: Vec<TextBuffer>,
    pub active_tab: usize,
    pub secondary_tab: Option<usize>,
    pub closed_tabs: Vec<TextBuffer>,
    pub split_enabled: bool,
    pub project_root: PathBuf,
    pub status: String,
    pub mode: Mode,
    pub command_buffer: String,
    pub search_buffer: String,
    pub goto_line_buffer: String,
    pub chat_input: String,
    pub conversation: ConversationState,
    pub ai_client: Option<CodexClient>,
    pub prediction_tx: Sender<PredictionResult>,
    pub prediction_rx: Receiver<PredictionResult>,
    pub chat_tx: Sender<ChatResult>,
    pub chat_rx: Receiver<ChatResult>,
    pub task_tx: Sender<TaskResult>,
    pub task_rx: Receiver<TaskResult>,
    pub prediction_generation: u64,
    pub active_prediction_generation: u64,
    pub last_edit: Option<Instant>,
    pub clipboard: String,
    pub explorer_visible: bool,
    pub ai_visible: bool,
    pub focus: FocusPane,
    pub state: AppState,
    pub ai_bootstrap_error: Option<String>,
    pub ai_scroll: usize,
    pub ai_follow_tail: bool,
    pub show_help: bool,
    pub reload_prompt: Option<PathBuf>,
    pub terminal_lines: Vec<String>,
    pub last_task: Option<TaskKind>,
    layout: Cell<UiLayout>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Command,
    Search,
    GoToLine,
    Chat,
    Help,
    ConfirmQuit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusPane {
    Editor,
    Explorer,
}

#[derive(Debug, Clone, Copy, Default)]
struct UiLayout {
    explorer: Option<Rect>,
    editor: Rect,
    editor_secondary: Option<Rect>,
    ai: Option<Rect>,
    ai_history: Option<Rect>,
    ai_input: Option<Rect>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
    Ignored,
}

#[derive(Debug)]
pub struct PredictionResult {
    pub generation: u64,
    pub suggestion: Result<String>,
}

#[derive(Debug)]
pub enum ChatResult {
    Started { prompt: String },
    Delta(String),
    Tool { call: String, output: String },
    Finished(Result<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Build,
    Test,
    Run,
}

#[derive(Debug)]
pub enum TaskResult {
    Started {
        label: String,
    },
    Finished {
        label: String,
        output: Result<String>,
    },
}

impl App {
    pub fn new(config: Config, initial_path: Option<PathBuf>) -> Result<Self> {
        let cwd = std::env::current_dir().context("failed to read current directory")?;
        let project_root = discover_project_root(&cwd);
        let state = config::load_state().unwrap_or_default();
        let mut config = config;
        if let Some(model) = state
            .codex_model
            .clone()
            .filter(|model| !model.trim().is_empty())
        {
            config.codex.model = model;
        }
        let mut tabs = Vec::new();
        if let Some(path) = initial_path {
            let buffer = if path.exists() {
                TextBuffer::open(&path)?
            } else {
                let mut buffer = TextBuffer::default();
                buffer.set_path(path);
                buffer
            };
            tabs.push(buffer);
        } else if !state.open_files.is_empty() {
            for path in state.open_files.iter().filter(|path| path.exists()) {
                if path.is_file() {
                    tabs.push(TextBuffer::open(path)?);
                }
            }
        } else if let Some(path) = state.last_file.as_ref().filter(|path| path.exists()) {
            tabs.push(TextBuffer::open(path)?);
        }
        if tabs.is_empty() {
            tabs.push(TextBuffer::default());
        }
        let active_tab = state.active_tab.min(tabs.len().saturating_sub(1));
        let secondary_tab = state
            .secondary_tab
            .filter(|index| *index < tabs.len() && *index != active_tab);
        let editor = tabs[active_tab].clone();
        let split_enabled = state.split_enabled && secondary_tab.is_some();

        let explorer_dir = state
            .last_dir
            .clone()
            .filter(|path| path.exists())
            .unwrap_or_else(|| project_root.clone());
        let explorer_entries = fs::read_entries(&explorer_dir, config.ui.show_hidden)?;
        let explorer_git_status = read_git_status(&project_root).unwrap_or_default();
        let explorer_selected = state
            .explorer_selected
            .min(explorer_entries.len().saturating_sub(1));
        let explorer_scroll = state
            .explorer_scroll
            .min(explorer_entries.len().saturating_sub(1));
        let (prediction_tx, prediction_rx) = mpsc::channel();
        let (chat_tx, chat_rx) = mpsc::channel();
        let (task_tx, task_rx) = mpsc::channel();
        let (ai_client, ai_bootstrap_error) = match CodexClient::from_config(&config.codex) {
            Ok(client) => (Some(client), None),
            Err(err) => (None, Some(err.to_string())),
        };

        Ok(Self {
            config,
            explorer_dir,
            explorer_entries,
            explorer_git_status,
            explorer_selected,
            explorer_scroll,
            editor,
            tabs,
            active_tab,
            secondary_tab,
            closed_tabs: Vec::new(),
            split_enabled,
            project_root,
            status: "flake ready".to_string(),
            mode: Mode::Normal,
            command_buffer: String::new(),
            search_buffer: String::new(),
            goto_line_buffer: String::new(),
            chat_input: String::new(),
            conversation: ConversationState::default(),
            ai_client,
            prediction_tx,
            prediction_rx,
            chat_tx,
            chat_rx,
            task_tx,
            task_rx,
            prediction_generation: 0,
            active_prediction_generation: 0,
            last_edit: None,
            clipboard: String::new(),
            explorer_visible: true,
            ai_visible: true,
            focus: FocusPane::Editor,
            state,
            ai_bootstrap_error,
            ai_scroll: 0,
            ai_follow_tail: true,
            show_help: false,
            reload_prompt: None,
            terminal_lines: vec!["flake ready".to_string()],
            last_task: None,
            layout: Cell::new(UiLayout::default()),
        })
    }

    pub fn run(mut self) -> Result<()> {
        crossterm::terminal::enable_raw_mode().context("failed to enable raw mode")?;
        let mut stdout = std::io::stdout();
        crossterm::execute!(
            stdout,
            crossterm::terminal::EnterAlternateScreen,
            crossterm::event::EnableMouseCapture
        )?;

        let backend = ratatui::backend::CrosstermBackend::new(stdout);
        let mut terminal = ratatui::Terminal::new(backend)?;

        let result = self.event_loop(&mut terminal);

        crossterm::terminal::disable_raw_mode().ok();
        let backend = terminal.backend_mut();
        crossterm::execute!(
            backend,
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::event::DisableMouseCapture
        )
        .ok();
        terminal.show_cursor().ok();

        self.persist_state().ok();
        result
    }

    fn event_loop<B: ratatui::backend::Backend>(
        &mut self,
        terminal: &mut ratatui::Terminal<B>,
    ) -> Result<()> {
        loop {
            while let Ok(result) = self.prediction_rx.try_recv() {
                if result.generation == self.prediction_generation {
                    match result.suggestion {
                        Ok(suggestion) => self.editor.set_suggestion(Some(suggestion)),
                        Err(_err) => self.editor.set_suggestion(None),
                    }
                }
            }

            while let Ok(result) = self.chat_rx.try_recv() {
                match result {
                    ChatResult::Started { prompt } => {
                        self.conversation.begin_turn(prompt);
                        self.follow_ai_tail();
                        self.status = "AI response streaming".to_string();
                    }
                    ChatResult::Delta(delta) => {
                        self.conversation.append_assistant_delta(&delta);
                    }
                    ChatResult::Tool { call, output } => {
                        self.push_chat_tool_result(call, output);
                    }
                    ChatResult::Finished(response) => match response {
                        Ok(text) => {
                            self.conversation.finish_turn_with_response(text);
                            self.status = "AI response received".to_string();
                            self.push_terminal("ai response received");
                            self.auto_save_agent_changes();
                        }
                        Err(err) => {
                            self.conversation.abort_turn();
                            self.status = self.describe_ai_error(&err);
                            self.push_terminal(self.status.clone());
                        }
                    },
                }
            }

            while let Ok(result) = self.task_rx.try_recv() {
                match result {
                    TaskResult::Started { label } => {
                        self.status = format!("running {}", label);
                        self.push_terminal(self.status.clone());
                    }
                    TaskResult::Finished { label, output } => match output {
                        Ok(text) => {
                            self.status = format!("{} complete", label);
                            self.push_terminal(self.status.clone());
                            for line in text.lines().take(40) {
                                self.push_terminal(line.to_string());
                            }
                        }
                        Err(err) => {
                            self.status = format!("{} failed: {}", label, err);
                            self.push_terminal(self.status.clone());
                        }
                    },
                }
            }

            self.check_external_changes()?;

            if self.should_fire_prediction() {
                self.spawn_prediction();
            }

            terminal.draw(|frame| self.render(frame))?;

            if event::poll(TICK_RATE)? {
                match event::read()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        if self.handle_key(key)? {
                            break;
                        }
                    }
                    Event::Key(_) => {}
                    Event::Mouse(mouse) => {
                        self.handle_mouse(mouse)?;
                    }
                    Event::Resize(_, _) => {}
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn should_fire_prediction(&self) -> bool {
        self.ai_client.is_some()
            && self.mode == Mode::Normal
            && self
                .last_edit
                .is_some_and(|edited_at| edited_at.elapsed() >= PREDICT_DEBOUNCE)
            && self.editor.suggestion.is_none()
            && self.active_prediction_generation != self.prediction_generation
    }

    fn spawn_prediction(&mut self) {
        let Some(client) = self.ai_client.clone() else {
            return;
        };
        let generation = self.prediction_generation;
        self.active_prediction_generation = generation;
        let tx = self.prediction_tx.clone();
        let ctx = PredictionContext {
            file_path: self
                .editor
                .path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "[untitled]".to_string()),
            language: self.editor.language_hint().to_string(),
            prefix: self.editor.prefix(),
            suffix: self.editor.suffix(),
        };

        thread::spawn(move || {
            let suggestion = client.predict_completion(ctx);
            let _ = tx.send(PredictionResult {
                generation,
                suggestion,
            });
        });
    }

    fn spawn_chat(&mut self, prompt: String) {
        let Some(client) = self.ai_client.clone() else {
            self.status = "Codex client not available".to_string();
            return;
        };
        self.follow_ai_tail();
        let tx = self.chat_tx.clone();
        let workspace = self.workspace_context();
        let root = self.explorer_dir.clone();
        let show_hidden = self.config.ui.show_hidden;
        thread::spawn(move || {
            let _ = tx.send(ChatResult::Started {
                prompt: prompt.clone(),
            });
            let mut transcript = String::new();
            let mut final_response: Result<String> =
                Err(anyhow::anyhow!("agent stopped without response"));
            for _ in 0..4 {
                let prompt_text = if transcript.is_empty() {
                    prompt.clone()
                } else {
                    format!(
                        "{}\n\nTool results so far:\n{}\n\nContinue from the prior answer and do not repeat completed tool calls.",
                        prompt, transcript
                    )
                };
                let mut stream_buffer = AgentStreamBuffer::default();
                let response = client.ask_stream(&prompt_text, &workspace, |delta| {
                    if let Some(text) = stream_buffer.push(delta) {
                        let _ = tx.send(ChatResult::Delta(text));
                    }
                });
                match response {
                    Ok(text) => {
                        if let Some(text) = stream_buffer.finish() {
                            let _ = tx.send(ChatResult::Delta(text));
                        }
                        let tool_calls = extract_agent_tool_calls(&text);
                        if tool_calls.is_empty() {
                            final_response = Ok(clean_agent_response(&text));
                            break;
                        }

                        transcript.push_str("assistant requested tools:\n");
                        transcript.push_str(&text);
                        transcript.push_str("\n\n");

                        for call in tool_calls {
                            let output = run_agent_tool_call(&root, show_hidden, &call)
                                .unwrap_or_else(|err| format!("tool error: {}", err));
                            transcript.push_str(&format!("TOOL {}\n{}\n\n", call, output));
                            let _ = tx.send(ChatResult::Tool {
                                call: call.clone(),
                                output,
                            });
                        }
                    }
                    Err(err) => {
                        final_response = Err(err);
                        break;
                    }
                }
            }
            let _ = tx.send(ChatResult::Finished(final_response));
        });
    }

    fn spawn_task(&mut self, kind: TaskKind) {
        let label = match kind {
            TaskKind::Build => "build",
            TaskKind::Test => "test",
            TaskKind::Run => "run",
        }
        .to_string();
        self.last_task = Some(kind);
        let tx = self.task_tx.clone();
        let root = self.project_root.clone();
        thread::spawn(move || {
            let _ = tx.send(TaskResult::Started {
                label: label.clone(),
            });
            let (program, args) = match kind {
                TaskKind::Build => ("cargo", vec!["build"]),
                TaskKind::Test => ("cargo", vec!["test"]),
                TaskKind::Run => ("cargo", vec!["run"]),
            };
            let output = Command::new(program)
                .args(args)
                .current_dir(root)
                .output()
                .map_err(|err| anyhow::anyhow!(err))
                .and_then(|output| {
                    let mut text = String::new();
                    text.push_str(&String::from_utf8_lossy(&output.stdout));
                    text.push_str(&String::from_utf8_lossy(&output.stderr));
                    if output.status.success() {
                        Ok(text)
                    } else {
                        Err(anyhow::anyhow!(text))
                    }
                });
            let _ = tx.send(TaskResult::Finished { label, output });
        });
    }

    fn workspace_context(&self) -> String {
        let mut text = format!("cwd: {}\n", self.explorer_dir.display());
        if let Some(path) = &self.editor.path {
            text.push_str(&format!("active_file: {}\n", path.display()));
        }
        text.push_str("open buffer:\n");
        for line in self.editor.lines.iter().take(120) {
            text.push_str(line);
            text.push('\n');
        }
        let conversation = self.conversation.lines();
        if !conversation.is_empty() {
            text.push_str("\nrecent chat:\n");
            for line in conversation.iter().rev().take(60).rev() {
                text.push_str(line);
                text.push('\n');
            }
        }
        text
    }

    fn sync_active_tab(&mut self) {
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            *tab = self.editor.clone();
        } else if self.tabs.is_empty() {
            self.tabs.push(self.editor.clone());
            self.active_tab = 0;
        }
    }

    fn buffer_index_for_path(&self, path: &Path) -> Option<usize> {
        self.tabs
            .iter()
            .position(|buffer| buffer.path.as_deref() == Some(path))
    }

    fn switch_to_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        self.active_tab = index;
        self.editor = self.tabs[index].clone();
        self.focus = FocusPane::Editor;
        self.mode = Mode::Normal;
        self.secondary_tab = if self.split_enabled && self.tabs.len() > 1 {
            self.tabs
                .iter()
                .enumerate()
                .find(|(idx, _)| *idx != self.active_tab)
                .map(|(idx, _)| idx)
        } else {
            None
        };
        self.scroll_cursor_into_view();
    }

    fn next_tab(&mut self) {
        if self.tabs.len() <= 1 {
            return;
        }
        let next = (self.active_tab + 1) % self.tabs.len();
        self.switch_to_tab(next);
    }

    fn prev_tab(&mut self) {
        if self.tabs.len() <= 1 {
            return;
        }
        let prev = if self.active_tab == 0 {
            self.tabs.len() - 1
        } else {
            self.active_tab - 1
        };
        self.switch_to_tab(prev);
    }

    fn open_buffer_in_tabs(&mut self, buffer: TextBuffer) {
        if let Some(path) = buffer.path.as_deref()
            && let Some(index) = self.buffer_index_for_path(path)
        {
            self.tabs[index] = buffer.clone();
            self.editor = buffer;
            self.active_tab = index;
            self.secondary_tab = self
                .split_enabled
                .then_some((index + 1) % self.tabs.len().max(1))
                .filter(|idx| *idx != self.active_tab);
            return;
        }
        self.tabs.push(buffer.clone());
        self.active_tab = self.tabs.len() - 1;
        self.editor = buffer;
        self.secondary_tab = self
            .split_enabled
            .then_some(0)
            .filter(|idx| *idx != self.active_tab);
    }

    fn open_path_in_tabs(&mut self, path: PathBuf) -> Result<()> {
        if path.exists() && path.is_dir() {
            self.explorer_dir = path;
            self.refresh_explorer()?;
            return Ok(());
        }
        let buffer = if path.exists() && path.is_file() {
            TextBuffer::open(&path)?
        } else {
            let mut buffer = TextBuffer::default();
            buffer.set_path(&path);
            buffer
        };
        self.open_buffer_in_tabs(buffer);
        self.state.last_file = self.editor.path.clone();
        Ok(())
    }

    fn close_active_tab(&mut self) {
        if self.tabs.len() <= 1 {
            self.editor = TextBuffer::default();
            self.tabs = vec![self.editor.clone()];
            self.active_tab = 0;
            self.secondary_tab = None;
            self.focus = FocusPane::Editor;
            self.mode = Mode::Normal;
            self.state.last_file = None;
            self.status = "closed buffer".to_string();
            self.push_terminal("closed buffer");
            return;
        }

        let removed = self.tabs.remove(self.active_tab);
        self.closed_tabs.push(removed);
        self.active_tab = self.active_tab.min(self.tabs.len() - 1);
        self.editor = self.tabs[self.active_tab].clone();
        self.secondary_tab = if self.split_enabled && self.tabs.len() > 1 {
            self.tabs
                .iter()
                .enumerate()
                .find(|(idx, _)| *idx != self.active_tab)
                .map(|(idx, _)| idx)
        } else {
            None
        };
        self.focus = FocusPane::Editor;
        self.mode = Mode::Normal;
        self.state.last_file = self.editor.path.clone();
        self.status = "closed buffer".to_string();
        self.push_terminal("closed buffer");
        self.sync_active_tab();
    }

    fn reopen_closed_tab(&mut self) {
        let Some(buffer) = self.closed_tabs.pop() else {
            self.status = "no closed buffer to reopen".to_string();
            return;
        };
        self.open_buffer_in_tabs(buffer);
        self.status = "reopened closed buffer".to_string();
        self.push_terminal("reopened closed buffer");
        self.sync_active_tab();
    }

    fn close_other_tabs(&mut self) {
        if self.tabs.len() <= 1 {
            self.status = "no other buffers".to_string();
            return;
        }
        let active = self.editor.clone();
        let closed = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(idx, _)| *idx != self.active_tab)
            .map(|(_, buffer)| buffer.clone())
            .collect::<Vec<_>>();
        self.closed_tabs.extend(closed);
        self.tabs = vec![active.clone()];
        self.active_tab = 0;
        self.editor = active;
        self.secondary_tab = None;
        self.split_enabled = false;
        self.status = "closed other buffers".to_string();
        self.push_terminal("closed other buffers");
        self.sync_active_tab();
    }

    fn toggle_split(&mut self) {
        self.split_enabled = !self.split_enabled;
        self.secondary_tab = if self.split_enabled && self.tabs.len() > 1 {
            self.tabs
                .iter()
                .enumerate()
                .find(|(idx, _)| *idx != self.active_tab)
                .map(|(idx, _)| idx)
        } else {
            None
        };
        self.status = if self.split_enabled {
            "split view enabled".to_string()
        } else {
            "split view disabled".to_string()
        };
        self.push_terminal(self.status.clone());
    }

    fn refresh_explorer(&mut self) -> Result<()> {
        self.explorer_entries = fs::read_entries(&self.explorer_dir, self.config.ui.show_hidden)?;
        self.explorer_git_status = read_git_status(&self.project_root).unwrap_or_default();
        self.explorer_selected = self
            .explorer_selected
            .min(self.explorer_entries.len().saturating_sub(1));
        self.explorer_scroll = self
            .explorer_scroll
            .min(self.explorer_entries.len().saturating_sub(1));
        self.ensure_explorer_visible(self.explorer_viewport_height());
        self.state.last_dir = Some(self.explorer_dir.clone());
        self.state.explorer_selected = self.explorer_selected;
        self.state.explorer_scroll = self.explorer_scroll;
        Ok(())
    }

    fn check_external_changes(&mut self) -> Result<()> {
        let mut prompt = None;
        for index in 0..self.tabs.len() {
            let Some(path) = self.tabs[index].path.clone() else {
                continue;
            };
            if !path.exists() || path.is_dir() {
                continue;
            }
            if self.tabs[index].is_modified_on_disk() {
                if self.tabs[index].dirty {
                    prompt = Some(path.clone());
                    break;
                }
                if self.tabs[index].refresh_from_disk()? {
                    if index == self.active_tab {
                        self.editor = self.tabs[index].clone();
                    }
                    self.status = format!("reloaded modified file {}", path.display());
                    self.push_terminal(self.status.clone());
                }
            }
        }

        if let Some(path) = prompt
            && self.reload_prompt.is_none()
        {
            self.reload_prompt = Some(path.clone());
            self.status = format!(
                "file changed on disk: {}  press R or Enter to reload",
                path.display()
            );
            self.push_terminal(self.status.clone());
        }
        Ok(())
    }

    fn open_entry(&mut self) -> Result<()> {
        let Some(entry) = self.explorer_entries.get(self.explorer_selected).cloned() else {
            return Ok(());
        };
        if entry.is_dir {
            self.explorer_dir = entry.path;
            self.refresh_explorer()?;
            return Ok(());
        }
        self.open_path_in_tabs(entry.path.clone())?;
        self.status = format!("opened {}", entry.path.display());
        self.push_terminal(self.status.clone());
        self.last_edit = Some(Instant::now());
        self.prediction_generation += 1;
        self.active_prediction_generation = 0;
        self.state.last_file = Some(entry.path);
        self.sync_active_tab();
        Ok(())
    }

    fn save(&mut self) -> Result<()> {
        self.editor.save()?;
        self.status = "saved".to_string();
        self.push_terminal("saved current buffer");
        self.state.last_file = self.editor.path.clone();
        self.sync_active_tab();
        Ok(())
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        if matches!(key.code, KeyCode::F(1)) {
            self.show_help = !self.show_help;
            self.mode = if self.show_help {
                Mode::Help
            } else {
                Mode::Normal
            };
            return Ok(false);
        }
        if self.reload_prompt.is_some() {
            match key.code {
                KeyCode::Enter | KeyCode::Char('r') | KeyCode::Char('R') => {
                    self.reload_current()?;
                    self.reload_prompt = None;
                    return Ok(false);
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                    self.reload_prompt = None;
                    self.status = "reload cancelled".to_string();
                    return Ok(false);
                }
                _ => {}
            }
        }
        if self.mode == Mode::ConfirmQuit {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => return Ok(true),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.mode = Mode::Normal;
                    self.status = "quit cancelled".to_string();
                    return Ok(false);
                }
                _ => return Ok(false),
            }
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
            if self.editor.dirty {
                self.mode = Mode::ConfirmQuit;
                self.status = "unsaved changes; press Y to quit or N to cancel".to_string();
                return Ok(false);
            }
            return Ok(true);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            self.save()?;
            return Ok(false);
        }
        if self.mode == Mode::Normal && self.focus == FocusPane::Editor {
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('z') | KeyCode::Char('Z'))
            {
                if self.editor.undo() {
                    self.sync_active_tab();
                    self.bump_edit();
                    self.status = "undo".to_string();
                }
                return Ok(false);
            }
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y'))
            {
                if self.editor.redo() {
                    self.sync_active_tab();
                    self.bump_edit();
                    self.status = "redo".to_string();
                }
                return Ok(false);
            }
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
            {
                self.copy_selection_or_line();
                return Ok(false);
            }
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('x') | KeyCode::Char('X'))
            {
                self.cut_selection_or_line();
                return Ok(false);
            }
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('v') | KeyCode::Char('V'))
            {
                self.paste_clipboard();
                return Ok(false);
            }
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::SHIFT)
                && matches!(key.code, KeyCode::Char('d') | KeyCode::Char('D'))
            {
                self.delete_current_line();
                return Ok(false);
            }
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('w') | KeyCode::Char('W'))
            {
                self.close_active_tab();
                return Ok(false);
            }
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('t') | KeyCode::Char('T'))
                && key.modifiers.contains(KeyModifiers::SHIFT)
            {
                self.reopen_closed_tab();
                return Ok(false);
            }
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('\\') {
                self.toggle_split();
                return Ok(false);
            }
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Tab {
                self.next_tab();
                return Ok(false);
            }
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('f') {
            self.mode = Mode::Search;
            if self.search_buffer.is_empty() {
                self.search_buffer = self.editor.current_line().to_string();
            }
            return Ok(false);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('l') {
            self.mode = Mode::GoToLine;
            self.goto_line_buffer.clear();
            return Ok(false);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('p') | KeyCode::Char('P'))
        {
            self.mode = Mode::Command;
            self.command_buffer.clear();
            return Ok(false);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('b') {
            self.explorer_visible = !self.explorer_visible;
            return Ok(false);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && key.modifiers.contains(KeyModifiers::SHIFT)
            && matches!(key.code, KeyCode::Char('d') | KeyCode::Char('D'))
        {
            self.duplicate_current_line();
            return Ok(false);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('g') | KeyCode::Char('G'))
        {
            let query = self.search_buffer.trim().to_string();
            if !query.is_empty() {
                let backwards = matches!(key.code, KeyCode::Char('G'));
                if self.find_and_jump(&query, backwards).is_some() {
                    self.status = if backwards {
                        format!("previous match for `{}`", query)
                    } else {
                        format!("next match for `{}`", query)
                    };
                } else {
                    self.status = format!("no matches for `{}`", query);
                }
            } else {
                self.status = "no search query".to_string();
            }
            return Ok(false);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('h') {
            self.config.ui.show_hidden = !self.config.ui.show_hidden;
            self.refresh_explorer()?;
            self.status = if self.config.ui.show_hidden {
                "show hidden files".to_string()
            } else {
                "hide hidden files".to_string()
            };
            return Ok(false);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('e') {
            self.focus = if self.explorer_visible {
                FocusPane::Explorer
            } else {
                FocusPane::Editor
            };
            return Ok(false);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('a') {
            self.ai_visible = !self.ai_visible;
            if self.ai_visible {
                self.follow_ai_tail();
            }
            return Ok(false);
        }

        match self.mode {
            Mode::Normal => self.handle_normal_mode(key)?,
            Mode::Command => self.handle_command_mode(key)?,
            Mode::Search => self.handle_search_mode(key)?,
            Mode::GoToLine => self.handle_goto_line_mode(key)?,
            Mode::Chat => self.handle_chat_mode(key)?,
            Mode::Help => self.handle_help_mode(key)?,
            Mode::ConfirmQuit => {}
        }
        Ok(false)
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> Result<()> {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.handle_mouse_click(mouse.column, mouse.row)?
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.handle_mouse_drag(mouse.column, mouse.row)?
            }
            MouseEventKind::ScrollDown => self.handle_mouse_scroll(mouse.column, mouse.row, -1)?,
            MouseEventKind::ScrollUp => self.handle_mouse_scroll(mouse.column, mouse.row, 1)?,
            _ => {}
        }
        Ok(())
    }

    fn handle_mouse_click(&mut self, column: u16, row: u16) -> Result<()> {
        let layout = self.layout.get();
        let point = Position::new(column, row);

        let editor_area = inner_rect(layout.editor);
        if editor_area.contains(point) {
            self.focus = FocusPane::Editor;
            self.mode = Mode::Normal;
            self.move_cursor_to_editor_point(point, editor_area);
            self.editor.begin_selection();
            self.editor.update_selection_to_cursor();
            return Ok(());
        }

        if let Some(secondary) = layout.editor_secondary {
            let secondary_area = inner_rect(secondary);
            if secondary_area.contains(point) {
                if let Some(index) = self.secondary_tab {
                    self.switch_to_tab(index);
                    self.focus = FocusPane::Editor;
                    self.mode = Mode::Normal;
                    self.move_cursor_to_editor_point(point, secondary_area);
                    self.editor.begin_selection();
                    self.editor.update_selection_to_cursor();
                }
                return Ok(());
            }
        }

        if let Some(explorer) = layout.explorer {
            let explorer_area = inner_rect(explorer);
            if explorer_area.contains(point) {
                self.focus = FocusPane::Explorer;
                self.mode = Mode::Normal;
                self.select_explorer_point(point, explorer_area)?;
                return Ok(());
            }
        }

        if let Some(ai) = layout.ai {
            let ai_area = inner_rect(ai);
            if ai_area.contains(point) {
                self.follow_ai_tail();
                self.mode = Mode::Chat;
                return Ok(());
            }
        }

        Ok(())
    }

    fn handle_mouse_drag(&mut self, column: u16, row: u16) -> Result<()> {
        let layout = self.layout.get();
        let point = Position::new(column, row);
        let editor_area = inner_rect(layout.editor);
        if editor_area.contains(point) {
            self.focus = FocusPane::Editor;
            self.mode = Mode::Normal;
            if self.editor.selection.is_none() {
                self.editor.begin_selection();
            }
            self.move_cursor_to_editor_point(point, editor_area);
            self.editor.update_selection_to_cursor();
        } else if let Some(secondary) = layout.editor_secondary {
            let secondary_area = inner_rect(secondary);
            if secondary_area.contains(point) {
                if let Some(index) = self.secondary_tab {
                    self.switch_to_tab(index);
                    self.focus = FocusPane::Editor;
                    self.mode = Mode::Normal;
                    if self.editor.selection.is_none() {
                        self.editor.begin_selection();
                    }
                    self.move_cursor_to_editor_point(point, secondary_area);
                    self.editor.update_selection_to_cursor();
                }
            }
        }
        Ok(())
    }

    fn handle_mouse_scroll(&mut self, column: u16, row: u16, delta: isize) -> Result<()> {
        let layout = self.layout.get();
        let point = Position::new(column, row);

        if let Some(ai) = layout.ai {
            let ai_area = inner_rect(ai);
            if ai_area.contains(point) && self.ai_visible {
                if delta > 0 {
                    self.scroll_ai_up(AI_SCROLL_STEP);
                } else {
                    self.scroll_ai_down(AI_SCROLL_STEP);
                }
                return Ok(());
            }
        }

        if let Some(explorer) = layout.explorer {
            let explorer_area = inner_rect(explorer);
            if explorer_area.contains(point) && self.explorer_visible {
                let len = self.explorer_entries.len();
                if len > 0 {
                    if delta > 0 {
                        self.explorer_selected = self.explorer_selected.saturating_sub(1);
                    } else {
                        self.explorer_selected =
                            self.explorer_selected.saturating_add(1).min(len - 1);
                    }
                    self.ensure_explorer_visible(explorer_area.height as usize);
                }
                return Ok(());
            }
        }

        let editor_area = inner_rect(layout.editor);
        if editor_area.contains(point) {
            if delta > 0 {
                self.editor.scroll_y = self.editor.scroll_y.saturating_sub(1);
            } else {
                self.editor.scroll_y = self.editor.scroll_y.saturating_add(1);
            }
            self.sync_active_tab();
        }
        Ok(())
    }

    fn move_cursor_to_editor_point(&mut self, point: Position, inner: Rect) {
        let x = point.x.saturating_sub(inner.x) as usize + self.editor.scroll_x;
        let y = point.y.saturating_sub(inner.y) as usize + self.editor.scroll_y;
        self.editor.cursor_y = y.min(self.editor.lines.len().saturating_sub(1));
        let line_len = self.editor.lines[self.editor.cursor_y].chars().count();
        self.editor.cursor_x = x.min(line_len);
        self.editor.clear_suggestion();
        self.sync_active_tab();
    }

    fn select_explorer_point(&mut self, point: Position, inner: Rect) -> Result<()> {
        let offset = point.y.saturating_sub(inner.y) as usize;
        let index = self.explorer_scroll.saturating_add(offset);
        self.explorer_selected = index.min(self.explorer_entries.len().saturating_sub(1));
        if let Some(entry) = self.explorer_entries.get(self.explorer_selected).cloned() {
            if entry.is_dir {
                self.explorer_dir = entry.path;
                self.refresh_explorer()?;
            } else {
                self.open_path_in_tabs(entry.path.clone())?;
                self.status = format!("opened {}", entry.path.display());
                self.state.last_file = Some(entry.path);
                self.focus = FocusPane::Editor;
                self.mode = Mode::Normal;
                self.sync_active_tab();
            }
        }
        self.ensure_explorer_visible(inner.height as usize);
        Ok(())
    }

    fn handle_normal_mode(&mut self, key: KeyEvent) -> Result<()> {
        if self.focus == FocusPane::Explorer && self.explorer_visible {
            return self.handle_explorer_mode(key);
        }
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Char(':') => {
                self.mode = Mode::Command;
                self.command_buffer.clear();
            }
            KeyCode::Char('n')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.new_buffer(None);
            }
            KeyCode::Char('r')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.reload_current()?;
            }
            KeyCode::Char('i') => {}
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.insert_text_with_selection(&c.to_string());
                self.bump_edit();
            }
            KeyCode::Enter => {
                self.insert_text_with_selection("\n");
                self.bump_edit();
            }
            KeyCode::Backspace => {
                if !self.delete_selection_if_any() {
                    self.editor.backspace();
                }
                self.bump_edit();
            }
            KeyCode::Delete => {
                if !self.delete_selection_if_any() {
                    self.editor.delete();
                }
                self.bump_edit();
            }
            KeyCode::Left => self.move_editor_with_selection(shift, |editor| editor.move_left()),
            KeyCode::Right => self.move_editor_with_selection(shift, |editor| editor.move_right()),
            KeyCode::Up => self.move_editor_with_selection(shift, |editor| editor.move_up()),
            KeyCode::Down => self.move_editor_with_selection(shift, |editor| editor.move_down()),
            KeyCode::Home => {
                self.move_editor_with_selection(shift, |editor| editor.move_line_start())
            }
            KeyCode::End => self.move_editor_with_selection(shift, |editor| editor.move_line_end()),
            KeyCode::Tab => {
                if self.editor.apply_suggestion() {
                    self.bump_edit();
                }
            }
            KeyCode::Esc => {
                self.editor.clear_suggestion();
                self.editor.clear_selection();
            }
            KeyCode::BackTab => {
                self.prev_tab();
            }
            KeyCode::F(5) => {
                self.mode = Mode::Chat;
                self.chat_input.clear();
                self.follow_ai_tail();
            }
            KeyCode::F(1) => {
                self.show_help = true;
                self.mode = Mode::Help;
            }
            _ => {}
        }
        self.editor.clamp_cursor();
        self.scroll_cursor_into_view();
        Ok(())
    }

    fn handle_explorer_mode(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc => self.focus = FocusPane::Editor,
            KeyCode::Up => {
                self.explorer_selected = self.explorer_selected.saturating_sub(1);
                self.ensure_explorer_visible(self.explorer_viewport_height());
            }
            KeyCode::Down => {
                self.explorer_selected =
                    (self.explorer_selected + 1).min(self.explorer_entries.len().saturating_sub(1));
                self.ensure_explorer_visible(self.explorer_viewport_height());
            }
            KeyCode::Enter => {
                self.open_entry()?;
                self.focus = FocusPane::Editor;
            }
            KeyCode::Backspace => {
                if let Some(parent) = self.explorer_dir.parent().map(Path::to_path_buf) {
                    self.explorer_dir = parent;
                    self.refresh_explorer()?;
                }
            }
            KeyCode::Char('r') => {
                self.refresh_explorer()?;
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_command_mode(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Tab => {
                self.autocomplete_command_buffer();
            }
            KeyCode::Enter => {
                let command = self.command_buffer.trim().to_string();
                self.mode = Mode::Normal;
                self.execute_command(&command)?;
            }
            KeyCode::Backspace => {
                self.command_buffer.pop();
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.command_buffer.push(c);
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_search_mode(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Enter => {
                self.apply_search_query();
                self.mode = Mode::Normal;
            }
            KeyCode::Backspace => {
                self.search_buffer.pop();
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.search_buffer.push(c);
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_goto_line_mode(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Enter => {
                if let Ok(line) = self.goto_line_buffer.trim().parse::<usize>() {
                    self.goto_line(line.saturating_sub(1));
                }
                self.mode = Mode::Normal;
            }
            KeyCode::Backspace => {
                self.goto_line_buffer.pop();
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                if c.is_ascii_digit() {
                    self.goto_line_buffer.push(c);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_chat_mode(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Tab => {
                self.autocomplete_chat_input();
            }
            KeyCode::PageUp => self.scroll_ai_up(AI_SCROLL_STEP * 4),
            KeyCode::PageDown => self.scroll_ai_down(AI_SCROLL_STEP * 4),
            KeyCode::Home => {
                self.ai_scroll = usize::MAX;
                self.ai_follow_tail = false;
            }
            KeyCode::End => self.follow_ai_tail(),
            KeyCode::Enter => {
                let prompt = self.chat_input.trim().to_string();
                if !prompt.is_empty() {
                    if prompt.starts_with('/') {
                        self.execute_chat_command(&prompt)?;
                    } else {
                        self.spawn_chat(prompt);
                    }
                }
                self.chat_input.clear();
            }
            KeyCode::Backspace => {
                self.chat_input.pop();
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.chat_input.push(c);
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_help_mode(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc | KeyCode::F(1) => {
                self.show_help = false;
                self.mode = Mode::Normal;
            }
            _ => {}
        }
        Ok(())
    }

    fn move_editor_with_selection<F>(&mut self, extend: bool, mut move_fn: F)
    where
        F: FnMut(&mut TextBuffer),
    {
        if extend {
            if self.editor.selection.is_none() {
                self.editor.begin_selection();
            }
            move_fn(&mut self.editor);
            self.editor.update_selection_to_cursor();
        } else {
            move_fn(&mut self.editor);
            self.editor.clear_selection();
        }
        self.sync_active_tab();
    }

    fn delete_selection_if_any(&mut self) -> bool {
        if self.editor.delete_selection() {
            true
        } else {
            false
        }
    }

    fn insert_text_with_selection(&mut self, text: &str) {
        if self.editor.has_selection() {
            self.editor.replace_selection_with(text);
        } else {
            self.editor.insert_str(text);
        }
    }

    fn copy_selection_or_line(&mut self) {
        if let Some(text) = self.editor.selected_text() {
            self.clipboard = text;
            self.status = "copied selection".to_string();
            self.push_terminal("copied selection");
            return;
        }

        self.clipboard = format!("{}\n", self.editor.current_line());
        self.status = "copied line".to_string();
        self.push_terminal("copied line");
    }

    fn cut_selection_or_line(&mut self) {
        if let Some(text) = self.editor.selected_text() {
            self.clipboard = text;
            if self.delete_selection_if_any() {
                self.bump_edit();
                self.status = "cut selection".to_string();
                self.push_terminal("cut selection");
            }
            return;
        }

        self.clipboard = format!("{}\n", self.editor.current_line());
        self.delete_current_line();
        self.status = "cut line".to_string();
        self.push_terminal("cut line");
    }

    fn paste_clipboard(&mut self) {
        if self.clipboard.is_empty() {
            self.status = "clipboard empty".to_string();
            return;
        }
        let clipboard = self.clipboard.clone();
        self.insert_text_with_selection(&clipboard);
        self.bump_edit();
        self.status = "pasted clipboard".to_string();
        self.push_terminal("pasted clipboard");
    }

    fn delete_current_line(&mut self) {
        if self.editor.lines.is_empty() {
            self.editor.lines.push(String::new());
        }
        if self.editor.lines.len() == 1 {
            self.editor.lines[0].clear();
            self.editor.cursor_x = 0;
            self.editor.cursor_y = 0;
        } else {
            let removed = self.editor.lines.remove(self.editor.cursor_y);
            if self.editor.cursor_y >= self.editor.lines.len() {
                self.editor.cursor_y = self.editor.lines.len().saturating_sub(1);
            }
            self.editor.cursor_x = self
                .editor
                .cursor_x
                .min(self.editor.current_line().chars().count());
            if removed.is_empty() {
                self.editor.cursor_x = 0;
            }
        }
        self.editor.clear_selection();
        self.editor.dirty = true;
        self.bump_edit();
        self.status = "deleted line".to_string();
        self.push_terminal("deleted line");
    }

    fn push_chat_tool_result(&mut self, tool: impl Into<String>, output: impl Into<String>) {
        let tool = tool.into();
        let output = output.into();
        self.conversation
            .push_tool_output(tool.clone(), output.clone());
        self.status = format!("{} complete", tool);
        self.push_terminal(format!("{} complete", tool));
        self.push_terminal(output.lines().next().unwrap_or("").to_string());
    }

    fn autocomplete_command_buffer(&mut self) {
        if let Some(completed) = complete_command_input(&self.command_buffer, command_candidates())
        {
            self.command_buffer = completed;
        }
    }

    fn autocomplete_chat_input(&mut self) {
        if !self.chat_input.trim_start().starts_with('/') {
            return;
        }
        if let Some(completed) = complete_command_input(&self.chat_input, chat_command_candidates())
        {
            self.chat_input = completed;
        }
    }

    fn scroll_ai_up(&mut self, lines: usize) {
        self.ai_follow_tail = false;
        self.ai_scroll = self.ai_scroll.saturating_add(lines);
    }

    fn scroll_ai_down(&mut self, lines: usize) {
        self.ai_follow_tail = false;
        self.ai_scroll = self.ai_scroll.saturating_sub(lines);
    }

    fn follow_ai_tail(&mut self) {
        self.ai_follow_tail = true;
        self.ai_scroll = 0;
    }

    fn resolve_tool_path(&self, target: &str) -> PathBuf {
        let trimmed = target.trim();
        if trimmed.is_empty() {
            return self.explorer_dir.clone();
        }
        let path = Path::new(trimmed);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.explorer_dir.join(path)
        }
    }

    fn list_directory_output(&self, path: &Path) -> Result<String> {
        let entries = fs::read_entries(path, self.config.ui.show_hidden)?;
        let mut output = format!("{}\n", path.display());
        let entries: Vec<_> = entries
            .into_iter()
            .filter(|entry| !entry.is_parent)
            .collect();
        if entries.is_empty() {
            output.push_str("  <empty>\n");
            return Ok(output);
        }
        for entry in entries {
            let name = if entry.is_dir {
                format!("{}/", entry.name)
            } else {
                entry.name
            };
            output.push_str("  ");
            output.push_str(&name);
            output.push('\n');
        }
        Ok(output)
    }

    fn tree_output(&self, path: &Path, depth: usize) -> Result<String> {
        let mut output = String::new();
        self.append_tree(path, depth, 0, &mut output)?;
        Ok(output)
    }

    fn append_tree(
        &self,
        path: &Path,
        depth: usize,
        indent: usize,
        output: &mut String,
    ) -> Result<()> {
        let prefix = "  ".repeat(indent);
        output.push_str(&format!("{}{}\n", prefix, path.display()));
        if depth == 0 || !path.is_dir() {
            return Ok(());
        }
        let entries = fs::read_entries(path, self.config.ui.show_hidden)?;
        for entry in entries {
            if entry.is_parent {
                continue;
            }
            if entry.is_dir {
                self.append_tree(&entry.path, depth - 1, indent + 1, output)?;
            } else {
                output.push_str(&format!("{}  {}\n", prefix, entry.name));
            }
        }
        Ok(())
    }

    fn read_file_output(&self, path: &Path) -> Result<String> {
        if !path.exists() {
            return Ok(format!("{} does not exist\n", path.display()));
        }
        if path.is_dir() {
            return Ok(format!("{} is a directory\n", path.display()));
        }
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read file {}", path.display()))?;
        Ok(format!("{}\n{}", path.display(), contents))
    }

    fn execute_chat_command(&mut self, command: &str) -> Result<()> {
        let mut parts = command[1..].split_whitespace();
        match parts.next().unwrap_or("") {
            "help" => {
                self.status = "chat commands: /help /clear /login /model NAME /open PATH /save /close /focus editor /focus explorer /focus ai /pwd /ls [PATH] /tree [PATH] /cat PATH /undo /redo /tab next /tab prev /split /close other /reopen closed /build /test /run /rerun /new [PATH] /reload /next /prev /editor /explorer /ai /quit".to_string();
            }
            "clear" => {
                self.conversation = ConversationState::default();
                self.follow_ai_tail();
                self.status = "chat cleared".to_string();
            }
            "login" => {
                self.status = "opening Codex login...".to_string();
                if let Err(err) = ai::login_and_save() {
                    self.status = format!("Codex login failed: {}", err);
                    self.push_terminal(self.status.clone());
                } else {
                    self.status = "Codex login complete".to_string();
                    self.reload_ai_client();
                    self.push_terminal(self.status.clone());
                }
            }
            "open" => {
                let target = parts.collect::<Vec<_>>().join(" ");
                if !target.is_empty() {
                    self.execute_command(&format!("open {}", target))?;
                }
            }
            "new" => {
                let target = parts.collect::<Vec<_>>().join(" ");
                if target.is_empty() {
                    self.new_buffer(None);
                } else {
                    self.new_buffer(Some(self.resolve_tool_path(&target)));
                }
            }
            "model" => {
                let model = parts.collect::<Vec<_>>().join(" ");
                self.set_codex_model(&model)?;
            }
            "save" => {
                self.save()?;
            }
            "reload" => {
                self.reload_current()?;
            }
            "close" => {
                self.close_active_tab();
            }
            "undo" => {
                if self.editor.undo() {
                    self.sync_active_tab();
                    self.bump_edit();
                    self.status = "undo".to_string();
                }
            }
            "redo" => {
                if self.editor.redo() {
                    self.sync_active_tab();
                    self.bump_edit();
                    self.status = "redo".to_string();
                }
            }
            "split" => {
                self.toggle_split();
            }
            "reopen" => {
                if parts.next() == Some("closed") {
                    self.reopen_closed_tab();
                }
            }
            "tab" => match parts.next().unwrap_or("") {
                "next" => self.next_tab(),
                "prev" => self.prev_tab(),
                _ => {}
            },
            "next" => self.next_tab(),
            "prev" => self.prev_tab(),
            "search" => {
                self.mode = Mode::Search;
                if self.search_buffer.is_empty() {
                    self.search_buffer = self.editor.current_line().to_string();
                }
            }
            "goto" => {
                self.mode = Mode::GoToLine;
                self.goto_line_buffer.clear();
            }
            "pwd" => {
                let output = format!("{}\n", self.explorer_dir.display());
                self.push_chat_tool_result("/pwd", output);
            }
            "ls" => {
                let target = parts.collect::<Vec<_>>().join(" ");
                let path = self.resolve_tool_path(&target);
                let output = self.list_directory_output(path.as_path())?;
                self.push_chat_tool_result(
                    if target.is_empty() {
                        "/ls".to_string()
                    } else {
                        format!("/ls {}", target)
                    },
                    output,
                );
            }
            "tree" => {
                let target = parts.collect::<Vec<_>>().join(" ");
                let path = self.resolve_tool_path(&target);
                let output = self.tree_output(path.as_path(), 3)?;
                self.push_chat_tool_result(
                    if target.is_empty() {
                        "/tree".to_string()
                    } else {
                        format!("/tree {}", target)
                    },
                    output,
                );
            }
            "cat" => {
                let target = parts.collect::<Vec<_>>().join(" ");
                if target.is_empty() {
                    self.status = "usage: /cat PATH".to_string();
                    return Ok(());
                }
                let path = self.resolve_tool_path(&target);
                let output = self.read_file_output(path.as_path())?;
                self.push_chat_tool_result(format!("/cat {}", target), output);
            }
            "build" => self.spawn_task(TaskKind::Build),
            "test" => self.spawn_task(TaskKind::Test),
            "run" => self.spawn_task(TaskKind::Run),
            "rerun" => {
                if let Some(kind) = self.last_task {
                    self.spawn_task(kind);
                } else {
                    self.status = "no previous task".to_string();
                }
            }
            "focus" => match parts.next().unwrap_or("") {
                "editor" => self.focus = FocusPane::Editor,
                "explorer" => self.focus = FocusPane::Explorer,
                "ai" => self.ai_visible = true,
                _ => {}
            },
            "editor" => self.focus = FocusPane::Editor,
            "explorer" => self.focus = FocusPane::Explorer,
            "ai" => self.ai_visible = true,
            "quit" => std::process::exit(0),
            _ => {
                self.status = format!("unknown chat command: {}", command);
            }
        }
        Ok(())
    }

    fn execute_command(&mut self, command: &str) -> Result<()> {
        match command {
            "q" | "quit" => std::process::exit(0),
            "w" | "save" => self.save()?,
            "open" => self.open_entry()?,
            "new" => self.new_buffer(None),
            "reload" => self.reload_current()?,
            "close" => self.close_active_tab(),
            "duplicate line" => self.duplicate_current_line(),
            "split" => self.toggle_split(),
            "close other" => self.close_other_tabs(),
            "reopen closed" => self.reopen_closed_tab(),
            "tab next" => self.next_tab(),
            "tab prev" => self.prev_tab(),
            "search" => {
                self.mode = Mode::Search;
                if self.search_buffer.is_empty() {
                    self.search_buffer = self.editor.current_line().to_string();
                }
            }
            "goto" => {
                self.mode = Mode::GoToLine;
                self.goto_line_buffer.clear();
            }
            "ai ask" => {
                self.mode = Mode::Chat;
                self.chat_input.clear();
            }
            "focus editor" => self.focus = FocusPane::Editor,
            "focus explorer" => self.focus = FocusPane::Explorer,
            "focus ai" => self.ai_visible = true,
            "build" => self.spawn_task(TaskKind::Build),
            "test" => self.spawn_task(TaskKind::Test),
            "run" => self.spawn_task(TaskKind::Run),
            "rerun" => {
                if let Some(kind) = self.last_task {
                    self.spawn_task(kind);
                } else {
                    self.status = "no previous task".to_string();
                }
            }
            "undo" => {
                if self.editor.undo() {
                    self.sync_active_tab();
                    self.bump_edit();
                    self.status = "undo".to_string();
                }
            }
            "redo" => {
                if self.editor.redo() {
                    self.sync_active_tab();
                    self.bump_edit();
                    self.status = "redo".to_string();
                }
            }
            "login" => {
                self.status = "opening Codex login...".to_string();
                if let Err(err) = ai::login_and_save() {
                    self.status = format!("Codex login failed: {}", err);
                    self.push_terminal(self.status.clone());
                } else {
                    self.status = "Codex login complete".to_string();
                    self.reload_ai_client();
                    self.push_terminal(self.status.clone());
                }
            }
            other if other.starts_with("model ") => {
                let model = other.trim_start_matches("model ").trim().to_string();
                self.set_codex_model(&model)?;
            }
            other if other.starts_with("open ") => {
                let target = other.trim_start_matches("open ").trim();
                let path = if Path::new(target).is_absolute() {
                    PathBuf::from(target)
                } else {
                    self.explorer_dir.join(target)
                };
                self.open_path_in_tabs(path)?;
                self.status = "opened path".to_string();
                self.push_terminal("opened path");
            }
            other if other.starts_with("new ") => {
                let target = other.trim_start_matches("new ").trim();
                let path = if target.is_empty() {
                    self.explorer_dir.join("untitled.txt")
                } else if Path::new(target).is_absolute() {
                    PathBuf::from(target)
                } else {
                    self.explorer_dir.join(target)
                };
                self.new_buffer(Some(path));
            }
            "help" => {
                self.show_help = true;
                self.mode = Mode::Help;
            }
            _ => {
                if !command.is_empty() {
                    self.status = format!("unknown command: {}", command);
                }
            }
        }
        Ok(())
    }

    fn set_codex_model(&mut self, model: &str) -> Result<()> {
        let model = model.trim();
        if model.is_empty() {
            self.status = "model name cannot be empty".to_string();
            return Ok(());
        }

        self.config.codex.model = model.to_string();
        self.state.codex_model = Some(model.to_string());
        self.reload_ai_client();
        self.persist_state()?;
        self.status = format!("codex model set to {}", model);
        self.push_terminal(format!("model set to {}", model));
        Ok(())
    }

    fn new_buffer(&mut self, path: Option<PathBuf>) {
        let path = path.unwrap_or_else(|| self.explorer_dir.join("untitled.txt"));
        let mut buf = TextBuffer::default();
        buf.set_path(path);
        self.open_buffer_in_tabs(buf);
        self.focus = FocusPane::Editor;
        self.mode = Mode::Normal;
        self.state.last_file = self.editor.path.clone();
        self.status = "new buffer".to_string();
        self.push_terminal("new buffer");
        self.sync_active_tab();
    }

    fn reload_current(&mut self) -> Result<()> {
        self.refresh_explorer()?;
        if let Some(path) = self.editor.path.clone() {
            if path.exists() && path.is_file() {
                let reopened = TextBuffer::open(path)?;
                self.editor = reopened;
                self.sync_active_tab();
                self.status = "reloaded file".to_string();
                self.push_terminal("reloaded file");
            }
        }
        Ok(())
    }

    fn duplicate_current_line(&mut self) {
        self.editor.duplicate_current_line();
        self.bump_edit();
        self.status = "duplicated line".to_string();
        self.sync_active_tab();
    }

    fn goto_line(&mut self, line_index: usize) {
        self.editor.cursor_y = line_index.min(self.editor.lines.len().saturating_sub(1));
        self.editor.cursor_x = self
            .editor
            .cursor_x
            .min(self.editor.lines[self.editor.cursor_y].chars().count());
        self.editor.clamp_cursor();
        self.scroll_cursor_into_view();
        self.status = format!("moved to line {}", self.editor.cursor_y + 1);
        self.sync_active_tab();
    }

    fn apply_search_query(&mut self) {
        let query = self.search_buffer.trim().to_string();
        if query.is_empty() {
            self.status = "search cleared".to_string();
            return;
        }
        if self.find_and_jump(&query, false).is_some() {
            let matches = self.find_matches(&query).len();
            self.status = format!("found `{}` ({} matches)", query, matches);
        } else {
            self.status = format!("no matches for `{}`", query);
        }
    }

    fn find_and_jump(&mut self, query: &str, backwards: bool) -> Option<(usize, usize)> {
        let matches = self.find_matches(query);
        let current = (self.editor.cursor_y, self.editor.cursor_x);
        let target = if backwards {
            matches
                .iter()
                .rev()
                .copied()
                .find(|&(y, x)| (y, x) < current)
                .or_else(|| matches.last().copied())
        } else {
            matches
                .iter()
                .copied()
                .find(|&(y, x)| (y, x) > current)
                .or_else(|| matches.first().copied())
        }?;
        self.editor.cursor_y = target.0;
        self.editor.cursor_x = target.1;
        self.editor.clamp_cursor();
        self.scroll_cursor_into_view();
        self.sync_active_tab();
        Some(target)
    }

    fn find_matches(&self, query: &str) -> Vec<(usize, usize)> {
        if query.is_empty() {
            return Vec::new();
        }
        let mut matches = Vec::new();
        for (y, line) in self.editor.lines.iter().enumerate() {
            let mut start = 0usize;
            while let Some(offset) = line[start..].find(query) {
                let x = start + offset;
                matches.push((y, x));
                start = x.saturating_add(query.len());
            }
        }
        matches
    }

    fn reload_ai_client(&mut self) {
        match CodexClient::from_config(&self.config.codex) {
            Ok(client) => {
                self.ai_client = Some(client);
                self.ai_bootstrap_error = None;
            }
            Err(err) => {
                self.ai_client = None;
                self.ai_bootstrap_error = Some(err.to_string());
            }
        }
    }

    fn explorer_viewport_height(&self) -> usize {
        self.layout
            .get()
            .explorer
            .map(|area| inner_rect(area).height as usize)
            .unwrap_or(0)
    }

    fn ensure_explorer_visible(&mut self, viewport_height: usize) {
        if viewport_height == 0 || self.explorer_entries.is_empty() {
            self.explorer_scroll = 0;
            return;
        }

        let max_scroll = self.explorer_entries.len().saturating_sub(viewport_height);
        if self.explorer_selected < self.explorer_scroll {
            self.explorer_scroll = self.explorer_selected;
        } else if self.explorer_selected >= self.explorer_scroll + viewport_height {
            self.explorer_scroll = self.explorer_selected.saturating_sub(viewport_height - 1);
        }

        self.explorer_scroll = self.explorer_scroll.min(max_scroll);
    }

    fn bump_edit(&mut self) {
        self.prediction_generation += 1;
        self.active_prediction_generation = 0;
        self.last_edit = Some(Instant::now());
        self.sync_active_tab();
    }

    fn push_terminal(&mut self, line: impl Into<String>) {
        self.terminal_lines.push(line.into());
        const MAX_LINES: usize = 24;
        if self.terminal_lines.len() > MAX_LINES {
            let overflow = self.terminal_lines.len() - MAX_LINES;
            self.terminal_lines.drain(0..overflow);
        }
    }

    fn auto_save_agent_changes(&mut self) {
        if !self.editor.dirty {
            return;
        }
        match self.save() {
            Ok(()) => {
                self.push_terminal("auto-saved agent changes");
            }
            Err(err) => {
                self.status = format!("auto-save failed: {}", err);
                self.push_terminal(self.status.clone());
            }
        }
    }

    fn scroll_cursor_into_view(&mut self) {
        let visible_rows = 20usize;
        let visible_cols = 80usize;
        if self.editor.cursor_y < self.editor.scroll_y {
            self.editor.scroll_y = self.editor.cursor_y;
        }
        if self.editor.cursor_y >= self.editor.scroll_y + visible_rows {
            self.editor.scroll_y = self.editor.cursor_y.saturating_sub(visible_rows - 1);
        }
        if self.editor.cursor_x < self.editor.scroll_x {
            self.editor.scroll_x = self.editor.cursor_x;
        }
        if self.editor.cursor_x >= self.editor.scroll_x + visible_cols {
            self.editor.scroll_x = self.editor.cursor_x.saturating_sub(visible_cols - 1);
        }
    }

    fn render(&self, frame: &mut Frame) {
        let size = frame.area();
        let body = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(5),
                Constraint::Length(3),
            ])
            .split(size);

        let layout = self.compute_layout(body.as_ref());
        self.layout.set(layout);

        self.render_header(frame, body[0]);
        self.render_body(frame, layout);
        self.render_footer(frame, body[2]);

        if self.mode == Mode::Command {
            self.render_overlay(
                frame,
                body[1],
                "Command",
                &self.command_buffer,
                "Type a command",
            );
        }
        if self.mode == Mode::Search {
            self.render_overlay(
                frame,
                body[1],
                "Search",
                &self.search_buffer,
                "Enter to search, Esc to cancel, Ctrl-G to continue",
            );
        }
        if self.mode == Mode::GoToLine {
            self.render_overlay(
                frame,
                body[1],
                "Go To Line",
                &self.goto_line_buffer,
                "Enter a line number and press Enter",
            );
        }
        if self.show_help {
            self.render_help(frame, body[1]);
        }
        if self.mode == Mode::ConfirmQuit {
            self.render_overlay(
                frame,
                body[1],
                "Confirm Quit",
                "Unsaved changes",
                "Press Y or Enter to quit, N or Esc to stay",
            );
        }
    }

    fn compute_layout(&self, body: &[Rect]) -> UiLayout {
        let mut columns = Vec::new();
        if self.explorer_visible {
            columns.push(Constraint::Length(self.config.ui.sidebar_width));
        }
        columns.push(Constraint::Min(20));
        if self.ai_visible {
            columns.push(Constraint::Length(self.config.ui.ai_width));
        }

        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(columns)
            .split(body[1]);
        let explorer = if self.explorer_visible {
            Some(split[0])
        } else {
            None
        };
        let editor_index = usize::from(self.explorer_visible);
        let editor = split[editor_index];
        let ai = if self.ai_visible {
            Some(split[split.len().saturating_sub(1)])
        } else {
            None
        };

        let editor_secondary = if self.split_enabled && self.secondary_tab.is_some() {
            if editor.width >= 40 {
                let parts = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                    .split(editor);
                Some(parts[1])
            } else {
                None
            }
        } else {
            None
        };

        let (ai_history, ai_input) = ai.map_or((None, None), |area| {
            let parts = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(3), Constraint::Length(4)])
                .split(area);
            (Some(parts[0]), Some(parts[1]))
        });

        UiLayout {
            explorer,
            editor,
            editor_secondary,
            ai,
            ai_history,
            ai_input,
        }
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let path = self
            .editor
            .path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "[untitled]".to_string());
        let tabs = self
            .tabs
            .iter()
            .enumerate()
            .map(|(idx, buffer)| {
                let name = buffer
                    .path
                    .as_ref()
                    .and_then(|path| path.file_name())
                    .and_then(|name| name.to_str())
                    .map(|name| name.to_string())
                    .unwrap_or_else(|| format!("untitled-{}", idx + 1));
                let label = if idx == self.active_tab {
                    format!("[{}]", name)
                } else {
                    format!(" {} ", name)
                };
                Span::styled(
                    label,
                    Style::default().fg(if idx == self.active_tab {
                        Color::Yellow
                    } else {
                        Color::DarkGray
                    }),
                )
            })
            .collect::<Vec<_>>();
        let text = Text::from(vec![
            Line::from(vec![
                Span::styled(
                    "flake ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("❄ "),
                Span::styled(path, Style::default().fg(Color::White)),
                Span::raw("  "),
                Span::styled(
                    if self.editor.dirty {
                        "modified"
                    } else {
                        "clean"
                    },
                    Style::default().fg(if self.editor.dirty {
                        Color::Yellow
                    } else {
                        Color::Green
                    }),
                ),
            ]),
            Line::from(tabs),
        ]);
        frame.render_widget(
            Paragraph::new(text).block(Block::default().borders(Borders::ALL)),
            area,
        );
    }

    fn render_body(&self, frame: &mut Frame, layout: UiLayout) {
        if let Some(explorer) = layout.explorer {
            self.render_explorer(frame, explorer);
        }
        if let Some(secondary_area) = layout.editor_secondary {
            self.render_editor(frame, layout.editor, &self.editor, true, self.active_tab);
            if let Some(index) = self.secondary_tab.and_then(|idx| self.tabs.get(idx)) {
                self.render_editor(
                    frame,
                    secondary_area,
                    index,
                    false,
                    self.secondary_tab.unwrap_or(0),
                );
            }
        } else {
            self.render_editor(frame, layout.editor, &self.editor, true, self.active_tab);
        }
        if let Some(ai) = layout.ai {
            self.render_ai(frame, ai, layout.ai_history, layout.ai_input);
        }
    }

    fn render_explorer(&self, frame: &mut Frame, area: Rect) {
        let title = if self.focus == FocusPane::Explorer {
            format!("Explorer* {}", self.explorer_dir.display())
        } else {
            format!("Explorer {}", self.explorer_dir.display())
        };
        let items: Vec<ListItem> = if self.explorer_entries.is_empty() {
            vec![ListItem::new("empty")]
        } else {
            let name_width = area.width.saturating_sub(6) as usize;
            self.explorer_entries
                .iter()
                .skip(self.explorer_scroll)
                .enumerate()
                .take(area.height.saturating_sub(2) as usize)
                .map(|(idx, entry)| {
                    let absolute_idx = self.explorer_scroll + idx;
                    let marker = if absolute_idx == self.explorer_selected {
                        ">"
                    } else {
                        " "
                    };
                    let name = if entry.is_dir {
                        if entry.is_parent {
                            entry.name.clone()
                        } else {
                            format!("{}/", entry.name)
                        }
                    } else {
                        entry.name.clone()
                    };
                    let git_marker = self
                        .explorer_git_status
                        .get(&entry.path)
                        .map(|status| match status {
                            GitStatus::Modified => "*",
                            GitStatus::Added => "+",
                            GitStatus::Deleted => "-",
                            GitStatus::Renamed => "r",
                            GitStatus::Untracked => "?",
                            GitStatus::Ignored => "!",
                        })
                        .unwrap_or(" ");
                    let clipped_name = clip_chars(&name, name_width.saturating_sub(1).max(1));
                    ListItem::new(Line::from(vec![
                        Span::styled(marker, Style::default().fg(Color::Cyan)),
                        Span::raw(" "),
                        Span::styled(git_marker, Style::default().fg(Color::Magenta)),
                        Span::raw(" "),
                        Span::styled(
                            clipped_name,
                            Style::default().fg(if entry.is_dir {
                                Color::Blue
                            } else {
                                Color::White
                            }),
                        ),
                    ]))
                })
                .collect()
        };
        frame.render_widget(
            List::new(items).block(Block::default().title(title).borders(Borders::ALL)),
            area,
        );
    }

    fn render_editor(
        &self,
        frame: &mut Frame,
        area: Rect,
        buffer: &TextBuffer,
        active: bool,
        tab_index: usize,
    ) {
        let title = match buffer.path.as_ref() {
            Some(path) => {
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("[untitled]");
                if active {
                    format!("{}* [{}]", "Editor", name)
                } else {
                    format!("{} [{}]", "Editor", name)
                }
            }
            None => {
                if active {
                    format!("Editor* [{}]", tab_index + 1)
                } else {
                    format!("Editor [{}]", tab_index + 1)
                }
            }
        };
        let inner = Block::default()
            .title(title.clone())
            .borders(Borders::ALL)
            .inner(area);
        frame.render_widget(Block::default().title(title).borders(Borders::ALL), area);
        let height = inner.height as usize;
        let width = inner.width as usize;
        let mut lines = Vec::new();
        let selection = buffer.selection_bounds();
        for (idx, line) in buffer
            .lines
            .iter()
            .enumerate()
            .skip(buffer.scroll_y)
            .take(height.saturating_sub(1))
        {
            lines.push(self.render_editor_line(buffer, idx, line, width, selection));
        }
        if lines.is_empty() {
            lines.push(Line::from(""));
        }
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);

        if active && self.mode == Mode::Normal && self.focus == FocusPane::Editor {
            let cursor_x = buffer
                .cursor_x
                .saturating_sub(buffer.scroll_x)
                .min(inner.width.saturating_sub(1) as usize) as u16;
            let cursor_y = buffer
                .cursor_y
                .saturating_sub(buffer.scroll_y)
                .min(inner.height.saturating_sub(1) as usize) as u16;
            frame.set_cursor_position(Position::new(inner.x + cursor_x, inner.y + cursor_y));
        }
    }

    fn render_editor_line(
        &self,
        buffer: &TextBuffer,
        line_index: usize,
        line: &str,
        width: usize,
        selection: Option<((usize, usize), (usize, usize))>,
    ) -> Line<'static> {
        let start = buffer.scroll_x;
        let chars: Vec<char> = line.chars().collect();
        let end = (start + width).min(chars.len());
        let visible: String = chars[start.min(chars.len())..end].iter().collect();
        let visible = clip_chars(&visible, width);
        let search_query = self.search_buffer.trim();

        if selection.is_none()
            && line_index == buffer.cursor_y
            && let Some(suggestion) = &buffer.suggestion
        {
            let mut text = visible;
            let cursor = buffer
                .cursor_x
                .saturating_sub(buffer.scroll_x)
                .min(text.chars().count());
            let suffix = suggestion.split('\n').next().unwrap_or("");
            let insert_at = text
                .char_indices()
                .nth(cursor)
                .map(|(idx, _)| idx)
                .unwrap_or(text.len());
            text.insert_str(insert_at, suffix);
            return Line::from(clip_chars(&text, width));
        }

        if selection.is_none() && line_index == buffer.cursor_y {
            if !search_query.is_empty()
                && let Some(match_start) = visible.find(search_query)
            {
                let mut spans = Vec::new();
                let before = &visible[..match_start];
                let after_match = &visible[match_start + search_query.len()..];
                if !before.is_empty() {
                    spans.push(Span::styled(
                        before.to_string(),
                        Style::default().bg(Color::DarkGray),
                    ));
                }
                spans.push(Span::styled(
                    search_query.to_string(),
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ));
                if !after_match.is_empty() {
                    spans.push(Span::styled(
                        after_match.to_string(),
                        Style::default().bg(Color::DarkGray),
                    ));
                }
                return Line::from(spans);
            }
            return Line::from(Span::styled(visible, Style::default().bg(Color::DarkGray)));
        }

        let selection_range =
            selection.and_then(|bounds| selection_range_for_line(bounds, line_index, chars.len()));
        if selection_range.is_none() {
            return Line::from(visible);
        }

        let mut spans = Vec::new();
        let mut current_selected = None;
        let mut segment = String::new();
        for (offset, ch) in chars
            .iter()
            .copied()
            .enumerate()
            .skip(start.min(chars.len()))
            .take(width)
        {
            let selected = selection_range
                .map(|(sel_start, sel_end)| offset >= sel_start && offset < sel_end)
                .unwrap_or(false);
            if current_selected == Some(selected) {
                segment.push(ch);
            } else {
                if !segment.is_empty() {
                    spans.push(styled_segment(
                        segment.clone(),
                        current_selected.unwrap_or(false),
                    ));
                    segment.clear();
                }
                segment.push(ch);
                current_selected = Some(selected);
            }
        }
        if !segment.is_empty() {
            spans.push(styled_segment(segment, current_selected.unwrap_or(false)));
        }
        if spans.is_empty() {
            spans.push(styled_segment(String::new(), false));
        }
        Line::from(spans)
    }

    fn render_ai(
        &self,
        frame: &mut Frame,
        area: Rect,
        history_area: Option<Rect>,
        input_area: Option<Rect>,
    ) {
        let mut history_lines: Vec<Line<'static>> = Vec::new();
        if let Some(suggestion) = &self.editor.suggestion {
            history_lines.push(Line::from(vec![Span::styled(
                "Suggestion",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )]));
            history_lines.push(Line::from(suggestion.clone()));
            history_lines.push(Line::from(""));
        }
        let conversation_lines = self.conversation.lines();
        if !conversation_lines.is_empty() {
            history_lines.push(Line::from(vec![Span::styled(
                if self.conversation.is_active() {
                    "Agent live"
                } else {
                    "Agent"
                },
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            )]));
            for line in conversation_lines {
                history_lines.push(Line::from(line));
            }
        }
        if history_lines.is_empty() {
            if self.ai_client.is_none() {
                history_lines.push(Line::from(
                    self.ai_bootstrap_error
                        .as_deref()
                        .map(|err| format!("Codex unavailable: {}", err))
                        .unwrap_or_else(|| "Codex unavailable. Run `codex login`.".to_string()),
                ));
            } else {
                history_lines.push(Line::from("No AI activity yet."));
                history_lines.push(Line::from("Ask for help below, or type /help."));
            }
        }

        let history_rect = history_area.unwrap_or(area);
        let history_inner = inner_rect(history_rect);
        let visible_lines = history_inner.height as usize;
        let max_scroll = history_lines.len().saturating_sub(visible_lines);
        let scroll = ai_history_scroll(max_scroll, self.ai_scroll, self.ai_follow_tail);
        let title = {
            let mut text = String::from("AI");
            if self.conversation.is_active() {
                text.push_str(" live");
            }
            if self.ai_follow_tail {
                text.push_str(" follow");
            } else if self.ai_scroll == usize::MAX {
                text.push_str(" top");
            } else if self.ai_scroll > 0 {
                text.push_str(&format!(" up {}", self.ai_scroll));
            }
            text
        };
        let history_text = Text::from(history_lines);
        let history_widget = Paragraph::new(history_text)
            .block(Block::default().title(title).borders(Borders::ALL))
            .wrap(Wrap { trim: false })
            .scroll((scroll as u16, 0));
        frame.render_widget(history_widget, history_rect);

        if let Some(input) = input_area {
            let block = Block::default().title("Chat").borders(Borders::ALL);
            let inner = block.inner(input);
            frame.render_widget(block, input);
            let prompt_hint = if self.chat_input.starts_with('/') {
                "command"
            } else {
                "prompt"
            };
            let input_text = if self.chat_input.is_empty() {
                format!("Type a {} or use /help", prompt_hint)
            } else {
                self.chat_input.clone()
            };
            let input_lines = Text::from(vec![
                Line::from(input_text),
                Line::from(Span::styled(
                    "Enter to send  Esc to close  PageUp/PageDown to scroll history  End to follow",
                    Style::default().fg(Color::DarkGray),
                )),
            ]);
            frame.render_widget(
                Paragraph::new(input_lines)
                    .style(Style::default().fg(Color::White))
                    .wrap(Wrap { trim: false }),
                inner,
            );
            if self.mode == Mode::Chat {
                let cursor_x = inner.x.saturating_add(self.chat_input.len() as u16);
                frame.set_cursor_position(Position::new(
                    cursor_x.min(inner.x + inner.width.saturating_sub(1)),
                    inner.y,
                ));
            }
        }
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        let current_mode = match self.mode {
            Mode::Normal => "normal",
            Mode::Command => "command",
            Mode::Search => "search",
            Mode::GoToLine => "goto",
            Mode::Chat => "chat",
            Mode::Help => "help",
            Mode::ConfirmQuit => "confirm",
        };
        let focus = match self.focus {
            FocusPane::Editor => "editor",
            FocusPane::Explorer => "explorer",
        };
        let suggestion = if self.editor.suggestion.is_some() {
            "tab to accept"
        } else {
            "no suggestion"
        };
        let selection = if self.editor.has_selection() {
            "selection active"
        } else {
            "no selection"
        };
        let mut lines = Vec::new();
        lines.push(Line::from(vec![
            Span::styled("flake>", Style::default().fg(Color::Cyan)),
            Span::raw(" "),
            Span::styled(
                current_mode,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                format!("model={}", self.config.codex.model),
                Style::default().fg(Color::Magenta),
            ),
            Span::raw(" "),
            Span::styled(format!("focus={}", focus), Style::default().fg(Color::Cyan)),
        ]));
        let recent = self
            .terminal_lines
            .last()
            .cloned()
            .unwrap_or_else(|| self.status.clone());
        lines.push(Line::from(vec![
            Span::styled(">", Style::default().fg(Color::Green)),
            Span::raw(" "),
            Span::raw(recent),
        ]));
        lines.push(Line::from(vec![
            Span::styled(">", Style::default().fg(Color::Green)),
            Span::raw(" "),
            Span::styled(
                format!(
                    "Ln {}, Col {}  {}  {}",
                    self.editor.cursor_y + 1,
                    self.editor.cursor_x + 1,
                    suggestion,
                    selection
                ),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }

    fn render_help(&self, frame: &mut Frame, area: Rect) {
        let popup_area = centered_rect(86, 72, area);
        frame.render_widget(Clear, popup_area);
        let lines = vec![
            Line::from(Span::styled(
                "Flake Help",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from("Navigation"),
            Line::from("  Ctrl-P / Ctrl-Shift-P  open command palette"),
            Line::from("  Ctrl-B                 toggle explorer"),
            Line::from("  Ctrl-H                 toggle hidden files"),
            Line::from("  Ctrl-E                 focus explorer"),
            Line::from("  Ctrl-F                 search in current file"),
            Line::from("  Ctrl-G                 next search match"),
            Line::from("  Ctrl-Shift-G           previous search match"),
            Line::from("  Ctrl-L                 go to line"),
            Line::from("  Ctrl-D                 delete current line"),
            Line::from("  Ctrl-Shift-D           duplicate current line"),
            Line::from("  Ctrl-C / Ctrl-X / Ctrl-V  copy, cut, and paste"),
            Line::from("  Shift+Arrows            select text"),
            Line::from("  Mouse drag             select text"),
            Line::from("  F5                     open AI chat"),
            Line::from("  F1                     toggle this help"),
            Line::from(""),
            Line::from("Editor"),
            Line::from("  Ctrl-S                 save"),
            Line::from("  Ctrl-N                 new buffer"),
            Line::from("  Ctrl-R                 reload current file"),
            Line::from("  Ctrl-Z / Ctrl-Y        undo and redo"),
            Line::from("  Ctrl-W                 close current buffer"),
            Line::from("  Ctrl-Shift-D           duplicate current line"),
            Line::from("  Ctrl-Tab               next buffer"),
            Line::from("  Ctrl-Shift-Tab         previous buffer"),
            Line::from("  Ctrl-\\\\                toggle split view"),
            Line::from("  Ctrl-Shift-T           reopen closed buffer"),
            Line::from("  Tab                    accept inline suggestion"),
            Line::from("  Ctrl-Q                 quit, confirm if modified"),
            Line::from(""),
            Line::from("AI"),
            Line::from("  model NAME             set Codex model"),
            Line::from("  /model NAME            set model from chat"),
            Line::from("  /login                 open Codex login"),
            Line::from("  /clear                 clear chat history"),
            Line::from("  /new [PATH]            create a buffer"),
            Line::from("  /reload                reload current file"),
            Line::from("  /close                 close buffer"),
            Line::from("  /pwd                   show current folder"),
            Line::from("  /ls [PATH]             list folder contents"),
            Line::from("  /tree [PATH]           show a folder tree"),
            Line::from("  /cat PATH              print a file"),
            Line::from("  /undo /redo            undo or redo"),
            Line::from("  /tab next / tab prev   cycle buffers"),
            Line::from("  /next /prev            cycle buffers"),
            Line::from("  /split                 toggle split view"),
            Line::from("  /close other           keep only active buffer"),
            Line::from("  /reopen closed         reopen a closed buffer"),
            Line::from("  /editor /explorer /ai  focus panes"),
            Line::from("  /build /test /run      run project tasks"),
            Line::from("  /rerun                 rerun last task"),
            Line::from("  /quit                  exit the app"),
            Line::from(""),
            Line::from("Press Esc or F1 to close."),
        ];
        frame.render_widget(
            Paragraph::new(lines)
                .block(Block::default().title("Help").borders(Borders::ALL))
                .wrap(Wrap { trim: false }),
            popup_area,
        );
    }

    fn render_overlay(&self, frame: &mut Frame, area: Rect, title: &str, input: &str, help: &str) {
        let popup_area = centered_rect(80, 20, area);
        frame.render_widget(Clear, popup_area);
        let block = Block::default().title(title).borders(Borders::ALL);
        let inner = block.inner(popup_area);
        frame.render_widget(block, popup_area);
        frame.render_widget(
            Paragraph::new(Text::from(vec![
                Line::from(input.to_string()),
                Line::from(Span::styled(help, Style::default().fg(Color::DarkGray))),
            ]))
            .wrap(Wrap { trim: false }),
            inner,
        );
    }

    fn persist_state(&mut self) -> Result<()> {
        self.state.last_dir = Some(self.explorer_dir.clone());
        self.state.open_files = self
            .tabs
            .iter()
            .filter_map(|buffer| buffer.path.clone())
            .collect();
        self.state.active_tab = self.active_tab;
        self.state.secondary_tab = self.secondary_tab;
        self.state.split_enabled = self.split_enabled;
        self.state.explorer_selected = self.explorer_selected;
        self.state.explorer_scroll = self.explorer_scroll;
        self.state.last_file = self.editor.path.clone();
        self.state.codex_model = Some(self.config.codex.model.clone());
        config::save_state(&self.state)
    }

    fn describe_ai_error(&self, err: &anyhow::Error) -> String {
        let text = err.to_string();
        if text.contains("model_not_supported") || text.contains("requested model is not supported")
        {
            return format!(
                "AI error: model {} is not supported; use `model NAME` to switch",
                self.config.codex.model
            );
        }

        if text.contains("401 Unauthorized") || text.contains("invalid_api_key") {
            return "AI error: Codex login missing or expired; run `codex login`".to_string();
        }

        if text.contains("403 Forbidden") {
            return format!(
                "AI error: 403 from Codex for model {}; check your OpenAI account access",
                self.config.codex.model
            );
        }

        if text.contains("stream did not contain text") {
            return format!(
                "AI error: Codex returned no text for model {}; try another model",
                self.config.codex.model
            );
        }

        format!("AI error: {}", text)
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

#[derive(Default)]
struct AgentStreamBuffer {
    pending: String,
}

impl AgentStreamBuffer {
    fn push(&mut self, delta: &str) -> Option<String> {
        self.pending.push_str(delta);
        let mut visible = String::new();
        loop {
            let Some(newline_index) = self.pending.find('\n') else {
                if self.pending.is_empty() {
                    break;
                }
                if self.pending.starts_with("TOOL ") || self.pending.starts_with("tool ") {
                    break;
                }
                visible.push_str(&self.pending);
                self.pending.clear();
                break;
            };
            let line = self.pending[..newline_index].to_string();
            self.pending.drain(..=newline_index);
            if parse_agent_tool_call(&line).is_none() {
                visible.push_str(&line);
                visible.push('\n');
            }
        }
        if visible.is_empty() {
            None
        } else {
            Some(visible)
        }
    }

    fn finish(&mut self) -> Option<String> {
        if self.pending.is_empty() {
            return None;
        }
        let line = std::mem::take(&mut self.pending);
        if parse_agent_tool_call(&line).is_none() {
            Some(line)
        } else {
            None
        }
    }
}

fn inner_rect(area: Rect) -> Rect {
    if area.width <= 2 || area.height <= 2 {
        return area;
    }
    Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width - 2,
        height: area.height - 2,
    }
}

fn clip_chars(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

fn ai_history_scroll(max_scroll: usize, ai_scroll: usize, follow_tail: bool) -> usize {
    if follow_tail {
        return max_scroll;
    }
    max_scroll.saturating_sub(ai_scroll.min(max_scroll))
}

fn extract_agent_tool_calls(text: &str) -> Vec<String> {
    text.lines().filter_map(parse_agent_tool_call).collect()
}

fn complete_command_input(input: &str, candidates: &[&str]) -> Option<String> {
    let prefix = input.trim_start();
    if prefix.is_empty() {
        return None;
    }

    let matches = candidates
        .iter()
        .copied()
        .filter(|candidate| candidate.starts_with(prefix))
        .collect::<Vec<_>>();
    if matches.is_empty() {
        return None;
    }

    let indent_len = input.len().saturating_sub(prefix.len());
    let indent = &input[..indent_len];
    if matches.len() == 1 {
        return Some(format!("{}{}", indent, matches[0]));
    }

    let mut common = matches[0].to_string();
    for candidate in &matches[1..] {
        common = common_prefix(&common, candidate);
        if common == prefix {
            break;
        }
    }

    if common.len() > prefix.len() {
        Some(format!("{}{}", indent, common))
    } else {
        Some(format!("{}{}", indent, matches[0]))
    }
}

fn common_prefix(a: &str, b: &str) -> String {
    a.chars()
        .zip(b.chars())
        .take_while(|(left, right)| left == right)
        .map(|(ch, _)| ch)
        .collect()
}

fn clean_agent_response(text: &str) -> String {
    let cleaned = text
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.starts_with("TOOL ") && !trimmed.starts_with("tool ")
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();
    if cleaned.is_empty() {
        "I could not parse the AI response. Try again.".to_string()
    } else {
        cleaned
    }
}

fn parse_agent_tool_call(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let call = trimmed
        .strip_prefix("TOOL ")
        .or_else(|| trimmed.strip_prefix("tool "))?
        .trim();
    let mut parts = call.split_whitespace();
    let command = parts.next()?;
    match command {
        "/pwd" | "pwd" => {
            if parts.next().is_some() {
                None
            } else {
                Some("/pwd".to_string())
            }
        }
        "/ls" | "ls" | "/tree" | "tree" => {
            let target = parts.collect::<Vec<_>>().join(" ");
            if target.split_whitespace().any(|part| {
                part.eq_ignore_ascii_case("TOOL") || part.to_ascii_lowercase().contains("tool")
            }) {
                return None;
            }
            if target.is_empty() {
                Some(format!("/{}", command.trim_start_matches('/')))
            } else {
                Some(format!("/{} {}", command.trim_start_matches('/'), target))
            }
        }
        "/cat" | "cat" => {
            let target = parts.collect::<Vec<_>>().join(" ");
            if target.is_empty()
                || target.split_whitespace().any(|part| {
                    part.eq_ignore_ascii_case("TOOL") || part.to_ascii_lowercase().contains("tool")
                })
            {
                None
            } else {
                Some(format!("/cat {}", target))
            }
        }
        _ => None,
    }
}

fn command_candidates() -> &'static [&'static str] {
    &[
        "help",
        "save",
        "open",
        "new",
        "reload",
        "close",
        "duplicate line",
        "split",
        "close other",
        "reopen closed",
        "tab next",
        "tab prev",
        "search",
        "goto",
        "ai ask",
        "focus editor",
        "focus explorer",
        "focus ai",
        "build",
        "test",
        "run",
        "rerun",
        "quit",
    ]
}

fn chat_command_candidates() -> &'static [&'static str] {
    &[
        "/help",
        "/clear",
        "/login",
        "/model ",
        "/open ",
        "/new ",
        "/save",
        "/reload",
        "/close",
        "/focus editor",
        "/focus explorer",
        "/focus ai",
        "/editor",
        "/explorer",
        "/ai",
        "/quit",
        "/pwd",
        "/ls ",
        "/tree ",
        "/cat ",
        "/undo",
        "/redo",
        "/tab next",
        "/tab prev",
        "/next",
        "/prev",
        "/split",
        "/close other",
        "/reopen closed",
        "/build",
        "/test",
        "/run",
        "/rerun",
        "/search",
        "/goto",
    ]
}

fn run_agent_tool_call(root: &Path, show_hidden: bool, call: &str) -> Result<String> {
    let mut parts = call.split_whitespace();
    match parts.next().unwrap_or("") {
        "/pwd" | "pwd" => Ok(format!("{}\n", root.display())),
        "/ls" | "ls" => {
            let target = parts.collect::<Vec<_>>().join(" ");
            let path = resolve_agent_tool_path(root, &target);
            list_directory_output(path.as_path(), show_hidden)
        }
        "/tree" | "tree" => {
            let target = parts.collect::<Vec<_>>().join(" ");
            let path = resolve_agent_tool_path(root, &target);
            tree_output(path.as_path(), show_hidden, 3)
        }
        "/cat" | "cat" => {
            let target = parts.collect::<Vec<_>>().join(" ");
            if target.is_empty() {
                return Ok("usage: /cat PATH\n".to_string());
            }
            let path = resolve_agent_tool_path(root, &target);
            read_file_output(path.as_path())
        }
        _ => Ok(format!("unknown tool command: {}\n", call)),
    }
}

fn resolve_agent_tool_path(root: &Path, target: &str) -> PathBuf {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return root.to_path_buf();
    }
    let path = Path::new(trimmed);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn list_directory_output(path: &Path, show_hidden: bool) -> Result<String> {
    let entries = fs::read_entries(path, show_hidden)?;
    let mut output = format!("{}\n", path.display());
    let entries: Vec<_> = entries
        .into_iter()
        .filter(|entry| !entry.is_parent)
        .collect();
    if entries.is_empty() {
        output.push_str("  <empty>\n");
        return Ok(output);
    }
    for entry in entries {
        let name = if entry.is_dir {
            format!("{}/", entry.name)
        } else {
            entry.name
        };
        output.push_str("  ");
        output.push_str(&name);
        output.push('\n');
    }
    Ok(output)
}

fn tree_output(path: &Path, show_hidden: bool, depth: usize) -> Result<String> {
    let mut output = String::new();
    append_tree(path, show_hidden, depth, 0, &mut output)?;
    Ok(output)
}

fn append_tree(
    path: &Path,
    show_hidden: bool,
    depth: usize,
    indent: usize,
    output: &mut String,
) -> Result<()> {
    let prefix = "  ".repeat(indent);
    output.push_str(&format!("{}{}\n", prefix, path.display()));
    if depth == 0 || !path.is_dir() {
        return Ok(());
    }
    let entries = fs::read_entries(path, show_hidden)?;
    for entry in entries {
        if entry.is_parent {
            continue;
        }
        if entry.is_dir {
            append_tree(&entry.path, show_hidden, depth - 1, indent + 1, output)?;
        } else {
            output.push_str(&format!("{}  {}\n", prefix, entry.name));
        }
    }
    Ok(())
}

fn read_file_output(path: &Path) -> Result<String> {
    if !path.exists() {
        return Ok(format!("{} does not exist\n", path.display()));
    }
    if path.is_dir() {
        return Ok(format!("{} is a directory\n", path.display()));
    }
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read file {}", path.display()))?;
    Ok(format!("{}\n{}", path.display(), contents))
}

fn selection_range_for_line(
    bounds: ((usize, usize), (usize, usize)),
    line_index: usize,
    line_len: usize,
) -> Option<(usize, usize)> {
    let ((start_x, start_y), (end_x, end_y)) = bounds;
    if line_index < start_y || line_index > end_y {
        return None;
    }

    let start = if line_index == start_y { start_x } else { 0 };
    let end = if line_index == end_y { end_x } else { line_len };
    Some((start.min(line_len), end.min(line_len)))
}

fn styled_segment(text: String, selected: bool) -> ratatui::text::Span<'static> {
    if selected {
        ratatui::text::Span::styled(
            text,
            Style::default()
                .fg(Color::White)
                .bg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        ratatui::text::Span::raw(text)
    }
}

fn discover_project_root(start: &Path) -> PathBuf {
    let mut current = Some(start);
    while let Some(path) = current {
        if path.join(".git").exists()
            || path.join("Cargo.toml").exists()
            || path.join("package.json").exists()
        {
            return path.to_path_buf();
        }
        current = path.parent();
    }
    start.to_path_buf()
}

fn read_git_status(root: &Path) -> Result<HashMap<PathBuf, GitStatus>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("status")
        .arg("--porcelain")
        .output();
    let Ok(output) = output else {
        return Ok(HashMap::new());
    };
    if !output.status.success() {
        return Ok(HashMap::new());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut statuses = HashMap::new();
    for line in text.lines() {
        if line.len() < 3 {
            continue;
        }
        let code = &line[..2];
        let path = line[3..].trim();
        let status = match code {
            "??" => GitStatus::Untracked,
            "A " | " M" => GitStatus::Added,
            " D" | "D " => GitStatus::Deleted,
            "R " => GitStatus::Renamed,
            "!!" => GitStatus::Ignored,
            _ => GitStatus::Modified,
        };
        statuses.insert(root.join(path), status);
    }
    Ok(statuses)
}

#[cfg(test)]
mod tests {
    use super::{
        ai_history_scroll, clean_agent_response, complete_command_input, extract_agent_tool_calls,
    };

    #[test]
    fn ai_history_scroll_follows_tail_when_enabled() {
        assert_eq!(ai_history_scroll(12, 7, true), 12);
    }

    #[test]
    fn ai_history_scroll_is_anchored_to_tail_when_detached() {
        assert_eq!(ai_history_scroll(12, 0, false), 12);
        assert_eq!(ai_history_scroll(12, 4, false), 8);
        assert_eq!(ai_history_scroll(12, 99, false), 0);
    }

    #[test]
    fn extracts_only_valid_agent_tool_calls() {
        let text = "\
TOOL /pwd
TOOL /pwdHey.
TOOL /ls ..
TOOL /ls ..TOOL /cat ../README.md
tool cat src/app.rs
TOOL /cat
";
        assert_eq!(
            extract_agent_tool_calls(text),
            vec!["/pwd", "/ls ..", "/cat src/app.rs"]
        );
    }

    #[test]
    fn clean_agent_response_hides_tool_protocol_lines() {
        assert_eq!(
            clean_agent_response("TOOL /pwd\nHere is the answer."),
            "Here is the answer."
        );
        assert_eq!(
            clean_agent_response("TOOL /pwdHey."),
            "I could not parse the AI response. Try again."
        );
    }

    #[test]
    fn command_completion_fills_unique_prefixes() {
        assert_eq!(
            complete_command_input("re", super::command_candidates()),
            Some("reload".to_string())
        );
        assert_eq!(
            complete_command_input("/f", super::chat_command_candidates()),
            Some("/focus ".to_string())
        );
    }

    #[test]
    fn agent_stream_buffer_hides_tool_lines() {
        let mut buffer = super::AgentStreamBuffer::default();
        assert_eq!(
            buffer.push("hello\nTOOL /pwd\nwor"),
            Some("hello\nwor".to_string())
        );
        assert_eq!(buffer.push("ld"), Some("ld".to_string()));
        assert_eq!(buffer.finish(), None);
    }
}

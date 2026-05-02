use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor_x: usize,
    pub anchor_y: usize,
    pub cursor_x: usize,
    pub cursor_y: usize,
}

#[derive(Debug, Clone)]
pub struct TextBuffer {
    pub path: Option<PathBuf>,
    pub lines: Vec<String>,
    pub cursor_x: usize,
    pub cursor_y: usize,
    pub scroll_x: usize,
    pub scroll_y: usize,
    pub dirty: bool,
    pub suggestion: Option<String>,
    pub selection: Option<Selection>,
    pub undo_stack: Vec<BufferSnapshot>,
    pub redo_stack: Vec<BufferSnapshot>,
    pub last_modified: Option<SystemTime>,
}

#[derive(Debug, Clone)]
pub struct BufferSnapshot {
    path: Option<PathBuf>,
    lines: Vec<String>,
    cursor_x: usize,
    cursor_y: usize,
    scroll_x: usize,
    scroll_y: usize,
    dirty: bool,
    suggestion: Option<String>,
    selection: Option<Selection>,
    last_modified: Option<SystemTime>,
}

impl Default for TextBuffer {
    fn default() -> Self {
        Self {
            path: None,
            lines: vec![String::new()],
            cursor_x: 0,
            cursor_y: 0,
            scroll_x: 0,
            scroll_y: 0,
            dirty: false,
            suggestion: None,
            selection: None,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            last_modified: None,
        }
    }
}

impl TextBuffer {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let contents = fs::read_to_string(&path)
            .with_context(|| format!("failed to read file {}", path.display()))?;
        let mut lines: Vec<String> = contents.lines().map(ToOwned::to_owned).collect();
        if contents.ends_with('\n') {
            lines.push(String::new());
        }
        if lines.is_empty() {
            lines.push(String::new());
        }
        let last_modified = fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .ok();

        Ok(Self {
            path: Some(path),
            lines,
            last_modified,
            ..Self::default()
        })
    }

    pub fn save(&mut self) -> Result<()> {
        let path = self
            .path
            .as_ref()
            .context("cannot save a buffer without a path")?;
        let mut contents = self.lines.join("\n");
        if !contents.ends_with('\n') {
            contents.push('\n');
        }
        fs::write(path, contents)
            .with_context(|| format!("failed to write file {}", path.display()))?;
        self.dirty = false;
        self.last_modified = fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok();
        Ok(())
    }

    pub fn set_path(&mut self, path: impl AsRef<Path>) {
        self.path = Some(path.as_ref().to_path_buf());
    }

    pub fn current_line(&self) -> &str {
        self.lines
            .get(self.cursor_y)
            .map(String::as_str)
            .unwrap_or("")
    }

    pub fn current_line_char_len(&self) -> usize {
        self.current_line().chars().count()
    }

    pub fn current_line_mut(&mut self) -> &mut String {
        if self.cursor_y >= self.lines.len() {
            self.lines.push(String::new());
        }
        &mut self.lines[self.cursor_y]
    }

    pub fn move_left(&mut self) {
        if self.cursor_x > 0 {
            self.cursor_x -= 1;
        } else if self.cursor_y > 0 {
            self.cursor_y -= 1;
            self.cursor_x = self.lines[self.cursor_y].chars().count();
        }
        self.clear_suggestion();
    }

    pub fn move_right(&mut self) {
        if self.cursor_x < self.current_line_char_len() {
            self.cursor_x += 1;
        } else if self.cursor_y + 1 < self.lines.len() {
            self.cursor_y += 1;
            self.cursor_x = 0;
        }
        self.clear_suggestion();
    }

    pub fn move_up(&mut self) {
        if self.cursor_y > 0 {
            self.cursor_y -= 1;
            self.cursor_x = self.cursor_x.min(self.current_line_char_len());
        }
        self.clear_suggestion();
    }

    pub fn move_down(&mut self) {
        if self.cursor_y + 1 < self.lines.len() {
            self.cursor_y += 1;
            self.cursor_x = self.cursor_x.min(self.current_line_char_len());
        }
        self.clear_suggestion();
    }

    pub fn move_line_start(&mut self) {
        self.cursor_x = 0;
        self.clear_suggestion();
    }

    pub fn move_line_end(&mut self) {
        self.cursor_x = self.current_line_char_len();
        self.clear_suggestion();
    }

    pub fn insert_str(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.has_selection() {
            let Some(((start_x, start_y), (end_x, end_y))) = self.selection_bounds() else {
                return;
            };
            self.record_undo();
            self.delete_selection_without_undo(start_x, start_y, end_x, end_y);
            self.insert_str_raw(text);
            return;
        }
        self.record_undo();
        self.insert_str_raw(text);
    }

    pub fn backspace(&mut self) {
        if self.has_selection() {
            self.delete_selection();
            return;
        }
        if self.cursor_x > 0 {
            self.record_undo();
            let remove_at = self.cursor_x - 1;
            let line_len = self.current_line_char_len();
            if remove_at < line_len {
                let line = self.current_line_mut();
                let start = char_to_byte_index(line, remove_at);
                let end = char_to_byte_index(line, remove_at + 1);
                line.replace_range(start..end, "");
                self.cursor_x -= 1;
                self.dirty = true;
            }
        } else if self.cursor_y > 0 {
            self.record_undo();
            let current = self.lines.remove(self.cursor_y);
            self.cursor_y -= 1;
            let new_x = self.current_line_char_len();
            self.current_line_mut().push_str(&current);
            self.cursor_x = new_x;
            self.dirty = true;
        }
        self.clear_suggestion();
    }

    pub fn delete(&mut self) {
        if self.has_selection() {
            self.delete_selection();
            return;
        }
        let line_len = self.current_line_char_len();
        if self.cursor_x < line_len {
            self.record_undo();
            let cursor_x = self.cursor_x;
            let line = self.current_line_mut();
            let start = char_to_byte_index(line, cursor_x);
            let end = char_to_byte_index(line, cursor_x + 1);
            line.replace_range(start..end, "");
            self.dirty = true;
        } else if self.cursor_y + 1 < self.lines.len() {
            self.record_undo();
            let next = self.lines.remove(self.cursor_y + 1);
            self.current_line_mut().push_str(&next);
            self.dirty = true;
        }
        self.clear_suggestion();
    }

    pub fn duplicate_current_line(&mut self) {
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }

        self.record_undo();
        let line = self.current_line().to_string();
        let insert_at = self.cursor_y + 1;
        let cursor_x = self.cursor_x.min(line.chars().count());
        self.lines.insert(insert_at, line);
        self.cursor_y = insert_at;
        self.cursor_x = cursor_x;
        self.dirty = true;
        self.selection = None;
        self.clear_suggestion();
    }

    pub fn apply_suggestion(&mut self) -> bool {
        let Some(suggestion) = self.suggestion.take() else {
            return false;
        };
        self.insert_str(&suggestion);
        true
    }

    pub fn set_suggestion(&mut self, suggestion: Option<String>) {
        self.suggestion = suggestion.filter(|s| !s.trim().is_empty());
    }

    pub fn clear_suggestion(&mut self) {
        self.suggestion = None;
    }

    pub fn begin_selection(&mut self) {
        self.selection = Some(Selection {
            anchor_x: self.cursor_x,
            anchor_y: self.cursor_y,
            cursor_x: self.cursor_x,
            cursor_y: self.cursor_y,
        });
    }

    pub fn update_selection_to_cursor(&mut self) {
        if let Some(selection) = self.selection.as_mut() {
            selection.cursor_x = self.cursor_x;
            selection.cursor_y = self.cursor_y;
        }
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    pub fn has_selection(&self) -> bool {
        self.selection_bounds().is_some()
    }

    pub fn selection_bounds(&self) -> Option<((usize, usize), (usize, usize))> {
        let selection = self.selection?;
        let anchor = (selection.anchor_y, selection.anchor_x);
        let cursor = (selection.cursor_y, selection.cursor_x);
        if anchor == cursor {
            return None;
        }
        if anchor <= cursor {
            Some(((anchor.1, anchor.0), (cursor.1, cursor.0)))
        } else {
            Some(((cursor.1, cursor.0), (anchor.1, anchor.0)))
        }
    }

    pub fn selected_text(&self) -> Option<String> {
        let ((start_x, start_y), (end_x, end_y)) = self.selection_bounds()?;
        if start_y >= self.lines.len() || end_y >= self.lines.len() {
            return None;
        }

        if start_y == end_y {
            return Some(slice_chars(&self.lines[start_y], start_x, end_x));
        }

        let mut text = String::new();
        text.push_str(&slice_chars(
            &self.lines[start_y],
            start_x,
            self.lines[start_y].chars().count(),
        ));
        text.push('\n');
        for line in self.lines.iter().take(end_y).skip(start_y + 1) {
            text.push_str(line);
            text.push('\n');
        }
        text.push_str(&slice_chars(&self.lines[end_y], 0, end_x));
        Some(text)
    }

    pub fn delete_selection(&mut self) -> bool {
        let Some(((start_x, start_y), (end_x, end_y))) = self.selection_bounds() else {
            return false;
        };
        self.record_undo();
        self.delete_selection_without_undo(start_x, start_y, end_x, end_y);
        true
    }

    pub fn replace_selection_with(&mut self, text: &str) {
        if self.has_selection() {
            self.record_undo();
            let Some(((start_x, start_y), (end_x, end_y))) = self.selection_bounds() else {
                return;
            };
            self.delete_selection_without_undo(start_x, start_y, end_x, end_y);
            self.insert_str_raw(text);
        } else {
            self.insert_str(text);
        }
    }

    pub fn prefix(&self) -> String {
        self.current_line()
            .chars()
            .take(self.cursor_x)
            .collect::<String>()
    }

    pub fn suffix(&self) -> String {
        self.current_line()
            .chars()
            .skip(self.cursor_x)
            .collect::<String>()
    }

    pub fn language_hint(&self) -> &'static str {
        match self
            .path
            .as_ref()
            .and_then(|path| path.extension())
            .and_then(|ext| ext.to_str())
            .unwrap_or("")
        {
            "rs" => "rust",
            "ts" => "typescript",
            "tsx" => "tsx",
            "js" => "javascript",
            "jsx" => "jsx",
            "lua" => "lua",
            "md" => "markdown",
            "toml" => "toml",
            "json" => "json",
            "yml" | "yaml" => "yaml",
            "sh" => "shell",
            _ => "text",
        }
    }

    pub fn clamp_cursor(&mut self) {
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.cursor_y = self.cursor_y.min(self.lines.len() - 1);
        self.cursor_x = self.cursor_x.min(self.lines[self.cursor_y].chars().count());
    }

    pub fn undo(&mut self) -> bool {
        let Some(snapshot) = self.undo_stack.pop() else {
            return false;
        };
        self.redo_stack.push(self.snapshot());
        self.restore(snapshot);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(snapshot) = self.redo_stack.pop() else {
            return false;
        };
        self.undo_stack.push(self.snapshot());
        self.restore(snapshot);
        true
    }

    pub fn is_modified_on_disk(&self) -> bool {
        let Some(path) = &self.path else {
            return false;
        };
        let Ok(metadata) = fs::metadata(path) else {
            return false;
        };
        let Ok(modified) = metadata.modified() else {
            return false;
        };
        self.last_modified.is_some_and(|known| known != modified)
    }

    pub fn refresh_from_disk(&mut self) -> Result<bool> {
        let Some(path) = self.path.clone() else {
            return Ok(false);
        };
        let Ok(metadata) = fs::metadata(&path) else {
            return Ok(false);
        };
        let Ok(modified) = metadata.modified() else {
            return Ok(false);
        };
        if self.last_modified == Some(modified) {
            return Ok(false);
        }
        let buffer = TextBuffer::open(&path)?;
        *self = buffer;
        Ok(true)
    }

    fn snapshot(&self) -> BufferSnapshot {
        BufferSnapshot {
            path: self.path.clone(),
            lines: self.lines.clone(),
            cursor_x: self.cursor_x,
            cursor_y: self.cursor_y,
            scroll_x: self.scroll_x,
            scroll_y: self.scroll_y,
            dirty: self.dirty,
            suggestion: self.suggestion.clone(),
            selection: self.selection,
            last_modified: self.last_modified,
        }
    }

    fn restore(&mut self, snapshot: BufferSnapshot) {
        self.path = snapshot.path;
        self.lines = snapshot.lines;
        self.cursor_x = snapshot.cursor_x;
        self.cursor_y = snapshot.cursor_y;
        self.scroll_x = snapshot.scroll_x;
        self.scroll_y = snapshot.scroll_y;
        self.dirty = snapshot.dirty;
        self.suggestion = snapshot.suggestion;
        self.selection = snapshot.selection;
        self.last_modified = snapshot.last_modified;
    }

    fn record_undo(&mut self) {
        self.undo_stack.push(self.snapshot());
        self.redo_stack.clear();
    }

    fn delete_selection_without_undo(
        &mut self,
        start_x: usize,
        start_y: usize,
        end_x: usize,
        end_y: usize,
    ) {
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }

        let start_y = start_y.min(self.lines.len() - 1);
        let end_y = end_y.min(self.lines.len() - 1);
        let start_x = start_x.min(self.lines[start_y].chars().count());
        let end_x = end_x.min(self.lines[end_y].chars().count());

        if start_y == end_y {
            let line = self.lines[start_y].clone();
            self.lines[start_y] = remove_char_range(&line, start_x, end_x);
        } else {
            let left = slice_chars(&self.lines[start_y], 0, start_x);
            let right = slice_chars(&self.lines[end_y], end_x, self.lines[end_y].chars().count());
            self.lines
                .splice(start_y..=end_y, [format!("{}{}", left, right)]);
        }

        if self.lines.is_empty() {
            self.lines.push(String::new());
        }

        self.cursor_y = start_y.min(self.lines.len() - 1);
        self.cursor_x = start_x.min(self.lines[self.cursor_y].chars().count());
        self.selection = None;
        self.dirty = true;
        self.clear_suggestion();
    }

    fn insert_str_raw(&mut self, text: &str) {
        for ch in text.chars() {
            if ch == '\n' {
                let split_at = {
                    let line_len = self.current_line_char_len();
                    self.cursor_x.min(line_len)
                };
                let line = self.current_line_mut();
                let split_byte = char_to_byte_index(line, split_at);
                let right = line.split_off(split_byte);
                self.lines.insert(self.cursor_y + 1, right);
                self.cursor_y += 1;
                self.cursor_x = 0;
            } else {
                let cursor_x = self.cursor_x;
                let line = self.current_line_mut();
                let byte_x = char_to_byte_index(line, cursor_x);
                line.insert(byte_x, ch);
                self.cursor_x = cursor_x + 1;
            }
            self.dirty = true;
            self.clear_suggestion();
        }
    }
}

fn slice_chars(text: &str, start: usize, end: usize) -> String {
    text.chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect()
}

fn remove_char_range(text: &str, start: usize, end: usize) -> String {
    let mut out = String::new();
    for (idx, ch) in text.chars().enumerate() {
        if idx < start || idx >= end {
            out.push(ch);
        }
    }
    out
}

fn char_to_byte_index(text: &str, char_idx: usize) -> usize {
    if char_idx == 0 {
        return 0;
    }
    text.char_indices()
        .nth(char_idx)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len())
}

#[cfg(test)]
mod tests {
    use super::TextBuffer;

    #[test]
    fn inserts_and_splits_lines() {
        let mut buffer = TextBuffer::default();
        buffer.insert_str("hello\nworld");
        assert_eq!(buffer.lines, vec!["hello".to_string(), "world".to_string()]);
    }

    #[test]
    fn backspace_merges_previous_line() {
        let mut buffer = TextBuffer::default();
        buffer.insert_str("hello\nworld");
        buffer.cursor_y = 1;
        buffer.cursor_x = 0;
        buffer.backspace();
        assert_eq!(buffer.lines, vec!["helloworld".to_string()]);
    }

    #[test]
    fn duplicate_current_line_inserts_below_and_keeps_cursor_column() {
        let mut buffer = TextBuffer::default();
        buffer.insert_str("hello\nworld");
        buffer.cursor_y = 0;
        buffer.cursor_x = 3;
        buffer.duplicate_current_line();
        assert_eq!(
            buffer.lines,
            vec![
                "hello".to_string(),
                "hello".to_string(),
                "world".to_string()
            ]
        );
        assert_eq!(buffer.cursor_y, 1);
        assert_eq!(buffer.cursor_x, 3);
    }

    #[test]
    fn deletes_selected_text_across_lines() {
        let mut buffer = TextBuffer::default();
        buffer.insert_str("hello\nworld\nagain");
        buffer.selection = Some(super::Selection {
            anchor_x: 2,
            anchor_y: 0,
            cursor_x: 3,
            cursor_y: 1,
        });
        assert!(buffer.delete_selection());
        assert_eq!(buffer.lines, vec!["held".to_string(), "again".to_string()]);
        assert_eq!(buffer.cursor_x, 2);
        assert_eq!(buffer.cursor_y, 0);
    }
}

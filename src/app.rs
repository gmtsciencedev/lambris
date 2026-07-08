use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use regex::{Regex, RegexBuilder};

use crate::data::Dataset;

/// What the keyboard is currently driving.
pub enum Mode {
    Normal,
    /// Typing into the search or filter prompt.
    Input(InputKind),
}

#[derive(Clone, Copy)]
pub enum InputKind {
    Search,
    Filter,
    /// Jump to a (1-based, original) row number.
    Goto,
}

/// An active search: the raw query plus its compiled regex.
pub struct Search {
    pub query: String,
    pub re: Regex,
}

/// UI state: selection, viewport, the current row view, and search/filter.
pub struct App {
    pub data: Dataset,
    /// Original row indices currently shown (all rows, or the filter result).
    pub rows: Vec<usize>,
    pub row_offset: usize,
    pub col_offset: usize,
    /// Selection is expressed against `rows`, not the underlying dataset.
    pub selected_row: usize,
    pub selected_col: usize,
    /// Rows of table body the last render could fit; used for paging.
    pub viewport_rows: usize,
    pub should_quit: bool,
    /// Set if a render pass failed; surfaced after the terminal is restored.
    pub render_error: Option<String>,

    pub mode: Mode,
    /// Buffer backing the prompt while in `Mode::Input`.
    pub input: String,
    pub search: Option<Search>,
    pub filter_query: Option<String>,
    /// Transient message shown in the status bar (errors, match info).
    pub status_msg: Option<String>,
    /// When set, the bottom line shows the selected column's info instead of
    /// the command hints (toggled with `i`).
    pub show_info: bool,
}

impl App {
    pub fn new(data: Dataset) -> Self {
        let rows = (0..data.nrows).collect();
        Self {
            data,
            rows,
            row_offset: 0,
            col_offset: 0,
            selected_row: 0,
            selected_col: 0,
            viewport_rows: 1,
            should_quit: false,
            render_error: None,
            mode: Mode::Normal,
            input: String::new(),
            search: None,
            filter_query: None,
            status_msg: None,
            show_info: false,
        }
    }

    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    fn last_row(&self) -> usize {
        self.row_count().saturating_sub(1)
    }

    fn last_col(&self) -> usize {
        self.data.ncols.saturating_sub(1)
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        match self.mode {
            Mode::Normal => self.handle_normal(key),
            Mode::Input(kind) => self.handle_input(key, kind),
        }
    }

    fn handle_normal(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.viewport_rows.max(1);
        self.status_msg = None;
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('c') if ctrl => self.should_quit = true,
            // Esc peels away search, then filter, then quits.
            KeyCode::Esc => {
                if self.search.take().is_some() {
                    self.status_msg = Some("search cleared".into());
                } else if self.filter_query.is_some() {
                    self.clear_filter();
                } else {
                    self.should_quit = true;
                }
            }

            KeyCode::Char('i') => self.show_info = !self.show_info,
            KeyCode::Char('/') => self.enter_input(InputKind::Search),
            KeyCode::Char('&') => self.enter_input(InputKind::Filter),
            KeyCode::Char(':') => self.enter_input(InputKind::Goto),
            KeyCode::Char('n') => self.jump_match(true),
            KeyCode::Char('N') => self.jump_match(false),

            KeyCode::Char('j') | KeyCode::Down => self.move_row(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_row(-1),
            KeyCode::Char('h') | KeyCode::Left => self.move_col(-1),
            KeyCode::Char('l') | KeyCode::Right => self.move_col(1),

            KeyCode::Char('d') if ctrl => self.move_row(page as isize / 2),
            KeyCode::Char('u') if ctrl => self.move_row(-(page as isize / 2)),
            KeyCode::Char('f') if ctrl => self.move_row(page as isize),
            KeyCode::Char('b') if ctrl => self.move_row(-(page as isize)),
            KeyCode::PageDown => self.move_row(page as isize),
            KeyCode::PageUp => self.move_row(-(page as isize)),

            KeyCode::Char('g') | KeyCode::Home => self.selected_row = 0,
            KeyCode::Char('G') | KeyCode::End => self.selected_row = self.last_row(),
            KeyCode::Char('0') | KeyCode::Char('^') => {
                self.selected_col = 0;
                self.col_offset = 0;
            }
            KeyCode::Char('$') => self.selected_col = self.last_col(),
            _ => {}
        }
    }

    fn handle_input(&mut self, key: KeyEvent, kind: InputKind) {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.input.clear();
            }
            KeyCode::Enter => self.commit_input(kind),
            KeyCode::Backspace => {
                self.input.pop();
            }
            // Goto only accepts digits; search/filter take any character.
            KeyCode::Char(c) => {
                if !matches!(kind, InputKind::Goto) || c.is_ascii_digit() {
                    self.input.push(c);
                }
            }
            _ => {}
        }
    }

    fn enter_input(&mut self, kind: InputKind) {
        self.mode = Mode::Input(kind);
        self.status_msg = None;
        self.input = match kind {
            InputKind::Search => self.search.as_ref().map(|s| s.query.clone()),
            InputKind::Filter => self.filter_query.clone(),
            InputKind::Goto => None,
        }
        .unwrap_or_default();
    }

    fn commit_input(&mut self, kind: InputKind) {
        let query = std::mem::take(&mut self.input);
        self.mode = Mode::Normal;
        if let InputKind::Goto = kind {
            self.goto_line(&query);
            return;
        }
        if query.is_empty() {
            match kind {
                InputKind::Search => self.search = None,
                InputKind::Filter => self.clear_filter(),
                InputKind::Goto => unreachable!(),
            }
            return;
        }
        let re = match build_regex(&query) {
            Ok(re) => re,
            Err(e) => {
                self.status_msg = Some(format!("bad pattern: {e}"));
                return;
            }
        };
        match kind {
            InputKind::Search => self.apply_search(query, re),
            InputKind::Filter => self.apply_filter(query, re),
            InputKind::Goto => unreachable!(),
        }
    }

    /// Jump to a 1-based original row number, honouring the active filter.
    fn goto_line(&mut self, text: &str) {
        if text.is_empty() || self.data.nrows == 0 {
            return;
        }
        let Ok(n) = text.parse::<usize>() else {
            self.status_msg = Some("invalid line number".into());
            return;
        };
        let target = n.saturating_sub(1).min(self.data.nrows - 1);
        match self.rows.iter().position(|&r| r == target) {
            Some(pos) => {
                self.selected_row = pos;
                self.status_msg = Some(format!("→ line {}", target + 1));
            }
            None => self.status_msg = Some(format!("line {n} not in current view")),
        }
    }

    fn apply_search(&mut self, query: String, re: Regex) {
        self.search = Some(Search { query, re });
        // Land on the first match from the current position (inclusive-ish).
        self.jump_match(true);
    }

    fn apply_filter(&mut self, query: String, re: Regex) {
        match self.data.filter_rows(&re) {
            Ok(rows) => {
                let n = rows.len();
                self.rows = rows;
                self.filter_query = Some(query);
                self.selected_row = 0;
                self.row_offset = 0;
                self.status_msg = Some(format!("{n} rows matched"));
            }
            Err(e) => self.status_msg = Some(format!("filter failed: {e}")),
        }
    }

    fn clear_filter(&mut self) {
        self.filter_query = None;
        self.rows = (0..self.data.nrows).collect();
        self.selected_row = self.selected_row.min(self.last_row());
        self.row_offset = 0;
        self.status_msg = Some("filter cleared".into());
    }

    fn jump_match(&mut self, forward: bool) {
        let Some(search) = &self.search else { return };
        match self
            .data
            .find_match(&search.re, &self.rows, self.selected_row, self.selected_col, forward)
        {
            Some((row, col)) => {
                self.selected_row = row;
                self.selected_col = col;
            }
            None => self.status_msg = Some("no match".into()),
        }
    }

    fn move_row(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        self.selected_row =
            (self.selected_row as isize + delta).clamp(0, self.last_row() as isize) as usize;
    }

    fn move_col(&mut self, delta: isize) {
        self.selected_col =
            (self.selected_col as isize + delta).clamp(0, self.last_col() as isize) as usize;
    }
}

/// Compile a user query as a case-insensitive regex (csvlens-style search).
fn build_regex(query: &str) -> Result<Regex, regex::Error> {
    RegexBuilder::new(query).case_insensitive(true).build()
}

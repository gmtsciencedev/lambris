use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use regex::{Regex, RegexBuilder};

use crate::data::Dataset;

/// Consecutive same-direction move events landing within this window are
/// treated as a held key and accelerate the scroll.
const REPEAT_WINDOW: Duration = Duration::from_millis(150);

/// Tracks a run of held-key repeats for one movement direction.
struct Repeat {
    /// Identifies the movement action, so reversing direction resets the run.
    id: u8,
    last: Instant,
    count: usize,
}

/// What the keyboard is currently driving.
pub enum Mode {
    Normal,
    /// Typing into the search or filter prompt.
    Input(InputKind),
}

#[derive(Clone, Copy)]
pub enum InputKind {
    /// Search across every column.
    Search,
    /// Search within the column selected when the prompt was opened.
    ColumnSearch,
    Filter,
    /// Jump to a (1-based, original) row number.
    Goto,
}

/// An active search: the raw query, its compiled regex, and its scope.
pub struct Search {
    pub query: String,
    pub re: Regex,
    /// `Some(col)` confines the search to one column; `None` searches all.
    pub scope: Option<usize>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SortDir {
    Asc,
    Desc,
}

/// The column the view is currently sorted by, and in which direction.
#[derive(Clone, Copy)]
pub struct SortSpec {
    pub col: usize,
    pub dir: SortDir,
}

/// UI state: selection, viewport, the current row view, and search/filter.
pub struct App {
    pub data: Dataset,
    /// Original row indices matching the current filter, in natural order.
    /// `rows` is derived from this by applying the active sort.
    base_rows: Vec<usize>,
    /// Original row indices currently shown (filter applied, then sorted).
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
    /// Number of leftmost columns pinned in place while scrolling horizontally.
    pub frozen_cols: usize,
    /// Active sort, if any.
    pub sort: Option<SortSpec>,
    /// State for held-key scroll acceleration.
    repeat: Option<Repeat>,
}

impl App {
    pub fn new(data: Dataset) -> Self {
        let base_rows: Vec<usize> = (0..data.nrows).collect();
        let rows = base_rows.clone();
        Self {
            data,
            base_rows,
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
            frozen_cols: 0,
            sort: None,
            repeat: None,
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
        self.handle_key_at(key, Instant::now());
    }

    /// `now` is injected so held-key acceleration can be tested deterministically.
    pub fn handle_key_at(&mut self, key: KeyEvent, now: Instant) {
        match self.mode {
            Mode::Normal => self.handle_normal(key, now),
            Mode::Input(kind) => self.handle_input(key, kind),
        }
    }

    fn handle_normal(&mut self, key: KeyEvent, now: Instant) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.viewport_rows.max(1) as isize;
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
            KeyCode::Char('s') => self.cycle_sort(),
            KeyCode::Char('f') if !ctrl => self.toggle_freeze(),
            KeyCode::Char('/') => self.enter_input(InputKind::Search),
            // Column-scoped search. `-` is a direct, unshifted key on AZERTY;
            // change this char to rebind.
            KeyCode::Char('-') => self.enter_input(InputKind::ColumnSearch),
            KeyCode::Char('&') => self.enter_input(InputKind::Filter),
            KeyCode::Char(':') => self.enter_input(InputKind::Goto),
            KeyCode::Char('n') => self.jump_match(true),
            KeyCode::Char('N') => self.jump_match(false),

            KeyCode::Char('j') | KeyCode::Down => self.move_row_accel(1, 1, now),
            KeyCode::Char('k') | KeyCode::Up => self.move_row_accel(2, -1, now),
            KeyCode::Char('h') | KeyCode::Left => self.move_col(-1),
            KeyCode::Char('l') | KeyCode::Right => self.move_col(1),

            KeyCode::Char('d') if ctrl => self.move_row_accel(3, page / 2, now),
            KeyCode::Char('u') if ctrl => self.move_row_accel(4, -(page / 2), now),
            KeyCode::Char('f') if ctrl => self.move_row_accel(5, page, now),
            KeyCode::Char('b') if ctrl => self.move_row_accel(6, -page, now),
            KeyCode::PageDown => self.move_row_accel(5, page, now),
            KeyCode::PageUp => self.move_row_accel(6, -page, now),

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
            InputKind::Search | InputKind::ColumnSearch => {
                self.search.as_ref().map(|s| s.query.clone())
            }
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
                InputKind::Search | InputKind::ColumnSearch => self.search = None,
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
            InputKind::Search => self.apply_search(query, re, None),
            InputKind::ColumnSearch => self.apply_search(query, re, Some(self.selected_col)),
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

    fn apply_search(&mut self, query: String, re: Regex, scope: Option<usize>) {
        self.search = Some(Search { query, re, scope });
        // Land on the first match from the current position (inclusive-ish).
        self.jump_match(true);
    }

    fn apply_filter(&mut self, query: String, re: Regex) {
        match self.data.filter_rows(&re) {
            Ok(rows) => {
                let n = rows.len();
                self.base_rows = rows;
                self.filter_query = Some(query);
                self.rebuild_rows();
                self.selected_row = 0;
                self.row_offset = 0;
                self.status_msg = Some(format!("{n} rows matched"));
            }
            Err(e) => self.status_msg = Some(format!("filter failed: {e}")),
        }
    }

    fn clear_filter(&mut self) {
        self.filter_query = None;
        self.base_rows = (0..self.data.nrows).collect();
        self.rebuild_rows();
        self.status_msg = Some("filter cleared".into());
    }

    /// Cycle the selected column through none → ascending → descending → none.
    fn cycle_sort(&mut self) {
        if self.data.ncols == 0 {
            return;
        }
        let col = self.selected_col;
        let next = match self.sort {
            Some(s) if s.col == col && s.dir == SortDir::Asc => Some(SortDir::Desc),
            Some(s) if s.col == col && s.dir == SortDir::Desc => None,
            _ => Some(SortDir::Asc),
        };
        self.sort = next.map(|dir| SortSpec { col, dir });
        self.rebuild_rows();
        self.status_msg = Some(match next {
            Some(SortDir::Asc) => format!("sorted ↑ {}", self.data.column_names[col]),
            Some(SortDir::Desc) => format!("sorted ↓ {}", self.data.column_names[col]),
            None => "sort cleared".into(),
        });
    }

    /// Pin columns `0..=selected` to the left, or unfreeze if already there.
    fn toggle_freeze(&mut self) {
        if self.data.ncols == 0 {
            return;
        }
        let want = self.selected_col + 1;
        if self.frozen_cols == want {
            self.frozen_cols = 0;
            self.status_msg = Some("columns unfrozen".into());
        } else {
            self.frozen_cols = want;
            self.status_msg = Some(format!("froze {want} column(s)"));
        }
    }

    /// Recompute `rows` from `base_rows` and the active sort, keeping the
    /// cursor on the same underlying record when it survives.
    fn rebuild_rows(&mut self) {
        let keep = self.rows.get(self.selected_row).copied();
        self.rows = match &self.sort {
            Some(s) => match self.data.sort_indices(&self.base_rows, s.col, s.dir == SortDir::Desc)
            {
                Ok(rows) => rows,
                Err(e) => {
                    self.status_msg = Some(format!("sort failed: {e}"));
                    return;
                }
            },
            None => self.base_rows.clone(),
        };
        self.selected_row = keep
            .and_then(|orig| self.rows.iter().position(|&r| r == orig))
            .unwrap_or(0)
            .min(self.last_row());
        self.row_offset = 0;
    }

    fn jump_match(&mut self, forward: bool) {
        let Some(search) = &self.search else { return };
        match self.data.find_match(
            &search.re,
            &self.rows,
            self.selected_row,
            self.selected_col,
            forward,
            search.scope,
        ) {
            Some((row, col)) => {
                self.selected_row = row;
                self.selected_col = col;
            }
            None => self.status_msg = Some("no match".into()),
        }
    }

    /// Move by `base` rows, scaled up while the same key is held down.
    /// `id` distinguishes movement directions so reversing resets acceleration.
    fn move_row_accel(&mut self, id: u8, base: isize, now: Instant) {
        let count = match &self.repeat {
            Some(r) if r.id == id && now.saturating_duration_since(r.last) <= REPEAT_WINDOW => {
                r.count + 1
            }
            _ => 0,
        };
        self.repeat = Some(Repeat { id, last: now, count });
        // Ramp: 1× for the first few presses, growing to a cap of 8×.
        let accel = 1 + (count / 3).min(7) as isize;
        self.move_row(base * accel);
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

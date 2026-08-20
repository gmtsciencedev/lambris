use std::collections::HashMap;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use regex::{Regex, RegexBuilder};

use crate::data::Dataset;
use crate::interrupt;

/// How a numeric column is displayed (set with `%`, `<`, `>`).
#[derive(Clone, Copy, Default)]
pub struct NumStyle {
    /// Align values on the decimal point.
    pub align: bool,
    /// Colour cells by the log magnitude of their value.
    pub log: bool,
    /// Fixed number of decimals; `None` keeps each value's natural form.
    pub decimals: Option<u8>,
}

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
    /// Open a file path in a new tab.
    Open,
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

/// The ordered set of original rows currently on display.
///
/// `All` is the identity view over `0..nrows` and stores nothing, so an
/// unfiltered, unsorted view of a billion-row file costs no memory. Only a
/// filter or sort materialises an explicit `Rows` permutation.
enum View {
    All,
    Rows(Vec<usize>),
}

impl View {
    fn len(&self, total: usize) -> usize {
        match self {
            View::All => total,
            View::Rows(v) => v.len(),
        }
    }

    /// The original row index shown at view position `i` (caller ensures range).
    fn orig(&self, i: usize) -> usize {
        match self {
            View::All => i,
            View::Rows(v) => v[i],
        }
    }

    /// The view position currently showing original row `orig`, if any.
    fn position(&self, orig: usize, total: usize) -> Option<usize> {
        match self {
            View::All => (orig < total).then_some(orig),
            View::Rows(v) => v.iter().position(|&r| r == orig),
        }
    }
}

/// UI state: selection, viewport, the current row view, and search/filter.
pub struct App {
    pub data: Dataset,
    /// Original row indices matching the active filter (`None` = all rows).
    filter: Option<Vec<usize>>,
    /// The current display order, derived from `filter` and `sort`.
    view: View,
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
    /// Whether the row-number gutter is shown (toggled with `#`).
    pub show_line_numbers: bool,
    /// This app is a transposed view (built on an in-memory transposed table).
    pub is_transposed: bool,
    /// Set when the user asks to transpose (handled by the main loop).
    pub transpose_request: bool,
    /// Set when a transposed view should be dismissed (handled by the loop).
    pub exit_transpose: bool,
    /// Set when the user asks for another tab: `+1` next, `-1` previous
    /// (handled by the main loop, which owns the set of tabs).
    pub switch_tab: Option<isize>,
    /// Set when the current tab should be closed (handled by the loop).
    pub close_tab: bool,
    /// A path typed at the `o` prompt, to be opened in a new tab by the loop.
    pub open_request: Option<String>,
    /// Set when the first row should switch between column names and data
    /// (handled by the loop, which reloads the file).
    pub toggle_header: bool,
    /// Number of leftmost columns pinned in place while scrolling horizontally.
    pub frozen_cols: usize,
    /// Active sort, if any.
    pub sort: Option<SortSpec>,
    /// Per-column numeric display styles, keyed by column index.
    pub num_styles: HashMap<usize, NumStyle>,
    /// State for held-key scroll acceleration.
    repeat: Option<Repeat>,
}

impl App {
    pub fn new(data: Dataset) -> Self {
        Self {
            data,
            filter: None,
            view: View::All,
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
            show_line_numbers: true,
            is_transposed: false,
            transpose_request: false,
            exit_transpose: false,
            switch_tab: None,
            close_tab: false,
            open_request: None,
            toggle_header: false,
            frozen_cols: 0,
            sort: None,
            num_styles: HashMap::new(),
            repeat: None,
        }
    }

    pub fn row_count(&self) -> usize {
        self.view.len(self.data.nrows)
    }

    /// Original dataset row index shown at view position `i`.
    pub fn orig_row(&self, i: usize) -> usize {
        self.view.orig(i)
    }

    /// Original dataset row index under the cursor.
    pub fn selected_orig(&self) -> usize {
        self.view.orig(self.selected_row)
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

    /// Original row indices of the current view, capped at `max` (used to build
    /// the transposed table without unbounded width on large files).
    pub fn view_rows(&self, max: usize) -> Vec<usize> {
        (0..self.row_count().min(max)).map(|i| self.orig_row(i)).collect()
    }

    fn handle_normal(&mut self, key: KeyEvent, now: Instant) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.viewport_rows.max(1) as isize;
        self.status_msg = None;
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('c') if ctrl => self.should_quit = true,
            // Esc peels away search, then filter, then leaves a transposed
            // view (or quits at the top level).
            KeyCode::Esc => {
                if self.search.take().is_some() {
                    self.status_msg = Some("search cleared".into());
                } else if self.filter_query.is_some() {
                    self.clear_filter();
                } else if self.is_transposed {
                    self.exit_transpose = true;
                } else {
                    self.should_quit = true;
                }
            }

            KeyCode::Char('i') => self.show_info = !self.show_info,
            KeyCode::Char('#') => self.show_line_numbers = !self.show_line_numbers,
            // `t` transposes; in a transposed view it returns to the original.
            KeyCode::Char('t') => {
                if self.is_transposed {
                    self.exit_transpose = true;
                } else {
                    self.transpose_request = true;
                }
            }
            // Tabs: cycle through the open files, close one, or open another.
            KeyCode::Tab => self.switch_tab = Some(1),
            KeyCode::BackTab => self.switch_tab = Some(-1),
            KeyCode::Char('w') if ctrl => self.close_tab = true,
            KeyCode::Char('o') => self.enter_input(InputKind::Open),
            // `T` re-reads the file with the first row as names or as data.
            KeyCode::Char('T') => {
                if self.is_transposed {
                    self.status_msg =
                        Some("header applies to the file — press t first".into());
                } else {
                    self.toggle_header = true;
                }
            }
            KeyCode::Char('%') => self.toggle_numeric(),
            KeyCode::Char('>') => self.adjust_decimals(1),
            KeyCode::Char('<') => self.adjust_decimals(-1),
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
            InputKind::Goto | InputKind::Open => None,
        }
        .unwrap_or_default();
    }

    fn commit_input(&mut self, kind: InputKind) {
        let query = std::mem::take(&mut self.input);
        self.mode = Mode::Normal;
        // These two take the text literally rather than as a regex.
        match kind {
            InputKind::Goto => return self.goto_line(&query),
            InputKind::Open => {
                let path = query.trim();
                if !path.is_empty() {
                    self.open_request = Some(path.to_string());
                }
                return;
            }
            _ => {}
        }
        if query.is_empty() {
            match kind {
                InputKind::Search | InputKind::ColumnSearch => self.search = None,
                InputKind::Filter => self.clear_filter(),
                InputKind::Goto | InputKind::Open => unreachable!(),
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
            InputKind::Goto | InputKind::Open => unreachable!(),
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
        match self.view.position(target, self.data.nrows) {
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
        match self.data.filter_rows(&re, interrupt::requested) {
            Ok(Some(rows)) => {
                let n = rows.len();
                self.filter = Some(rows);
                self.filter_query = Some(query);
                self.selected_row = 0;
                self.row_offset = 0;
                if self.rebuild_view() {
                    self.status_msg = Some(format!("{n} rows matched"));
                } else {
                    // Filtering succeeded but re-sorting the subset was aborted.
                    self.drop_sort_after_cancel();
                    self.status_msg = Some(format!("{n} rows matched; sort cancelled"));
                }
            }
            Ok(None) => {
                interrupt::take();
                self.status_msg = Some("filter cancelled".into());
            }
            Err(e) => self.status_msg = Some(format!("filter failed: {e}")),
        }
    }

    fn clear_filter(&mut self) {
        self.filter_query = None;
        self.filter = None;
        if self.rebuild_view() {
            self.status_msg = Some("filter cleared".into());
        } else {
            self.drop_sort_after_cancel();
            self.status_msg = Some("filter cleared; sort cancelled".into());
        }
    }

    /// Recover after a cancelled sort: drop the sort and rebuild the (now
    /// cheap, sort-free) view so state stays consistent.
    fn drop_sort_after_cancel(&mut self) {
        interrupt::take();
        self.sort = None;
        self.rebuild_view();
    }

    fn cycle_sort(&mut self) {
        self.cycle_sort_col(self.selected_col);
    }

    /// Cycle `col` through none → ascending → descending → none.
    fn cycle_sort_col(&mut self, col: usize) {
        if self.data.ncols == 0 {
            return;
        }
        let prev = self.sort;
        let next = match self.sort {
            Some(s) if s.col == col && s.dir == SortDir::Asc => Some(SortDir::Desc),
            Some(s) if s.col == col && s.dir == SortDir::Desc => None,
            _ => Some(SortDir::Asc),
        };
        self.sort = next.map(|dir| SortSpec { col, dir });
        if !self.rebuild_view() {
            // Sort aborted; restore the previous ordering (the view is untouched).
            interrupt::take();
            self.sort = prev;
            self.status_msg = Some("sort cancelled".into());
            return;
        }
        self.status_msg = Some(match next {
            Some(SortDir::Asc) => format!("sorted ↑ {}", self.data.column_names[col]),
            Some(SortDir::Desc) => format!("sorted ↓ {}", self.data.column_names[col]),
            None => "sort cleared".into(),
        });
    }

    fn toggle_numeric(&mut self) {
        self.toggle_numeric_col(self.selected_col);
    }

    /// Toggle decimal-aligned, log-coloured numeric display on `col`. Turning
    /// the colour off with no fixed decimals reverts to plain.
    fn toggle_numeric_col(&mut self, col: usize) {
        if !self.data.is_numeric(col) {
            self.status_msg = Some("column is not numeric".into());
            return;
        }
        let mut st = self.num_styles.get(&col).copied().unwrap_or_default();
        st.log = !st.log;
        st.align = st.log || st.decimals.is_some();
        if st.align {
            self.status_msg = Some(if st.log {
                "numeric + log colour".into()
            } else {
                "numeric".into()
            });
            self.num_styles.insert(col, st);
        } else {
            self.num_styles.remove(&col);
            self.status_msg = Some("plain".into());
        }
    }

    fn adjust_decimals(&mut self, delta: isize) {
        self.adjust_decimals_col(self.selected_col, delta);
    }

    /// Adjust the fixed decimal count on `col`, enabling decimal alignment
    /// (but not colouring).
    fn adjust_decimals_col(&mut self, col: usize, delta: isize) {
        if !self.data.is_numeric(col) {
            self.status_msg = Some("column is not numeric".into());
            return;
        }
        let mut st = self.num_styles.get(&col).copied().unwrap_or_default();
        let current = st.decimals.unwrap_or(2) as isize;
        let n = (current + delta).clamp(0, 10) as u8;
        st.decimals = Some(n);
        st.align = true;
        self.num_styles.insert(col, st);
        self.status_msg = Some(format!("{n} decimals"));
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

    /// Recompute the view from the active filter and sort, keeping the cursor
    /// on the same underlying record when it survives. Returns `false` if a
    /// sort was cancelled — in which case the view is left untouched, so the
    /// caller can revert whatever state it changed.
    ///
    /// With no filter and no sort the view is `All` (no allocation). A sort
    /// materialises a permutation — over the filtered subset, or over all rows
    /// when unfiltered (unavoidable: the result *is* an explicit ordering).
    fn rebuild_view(&mut self) -> bool {
        let keep = (self.row_count() > 0).then(|| self.selected_orig());
        let new_view = match (&self.filter, self.sort) {
            (None, None) => View::All,
            (Some(f), None) => View::Rows(f.clone()),
            (filter, Some(s)) => {
                let base: Vec<usize> = match filter {
                    Some(f) => f.clone(),
                    None => (0..self.data.nrows).collect(),
                };
                match self.data.sort_indices(&base, s.col, s.dir == SortDir::Desc, interrupt::requested)
                {
                    Ok(Some(rows)) => View::Rows(rows),
                    Ok(None) => return false, // cancelled; leave the view as it was
                    Err(e) => {
                        // Rare (e.g. incomparable type): drop the sort, show why.
                        self.status_msg = Some(format!("sort failed: {e}"));
                        self.sort = None;
                        match &self.filter {
                            Some(f) => View::Rows(f.clone()),
                            None => View::All,
                        }
                    }
                }
            }
        };
        self.view = new_view;
        self.selected_row = keep
            .and_then(|orig| self.view.position(orig, self.data.nrows))
            .unwrap_or(0)
            .min(self.last_row());
        self.row_offset = 0;
        true
    }

    fn jump_match(&mut self, forward: bool) {
        let Some(search) = &self.search else { return };
        let found = self.data.find_match(
            &search.re,
            self.row_count(),
            |i| self.view.orig(i),
            self.selected_row,
            self.selected_col,
            forward,
            search.scope,
            interrupt::requested,
        );
        match found {
            Some((row, col)) => {
                self.selected_row = row;
                self.selected_col = col;
            }
            None if interrupt::take() => self.status_msg = Some("search cancelled".into()),
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
        if self.row_count() == 0 {
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

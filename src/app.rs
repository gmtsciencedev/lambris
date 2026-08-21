use std::collections::HashMap;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use regex::{Regex, RegexBuilder};

use crate::browse::Completions;
use crate::data::{Dataset, SortKey, SortMethod};
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

/// Bounds on how wide a column is drawn. `MIN`/`MAX_COL_WIDTH` bound the width
/// a column derives from its contents, so one wide column cannot push
/// everything else off screen. A width set by hand (`r`/`R`) is bounded by
/// `MIN`/`MAX_SET_WIDTH` instead, which is far more generous — the point of
/// widening by hand is to see a long value.
pub const MIN_COL_WIDTH: u16 = 3;
pub const MAX_COL_WIDTH: u16 = 40;
pub const MIN_SET_WIDTH: u16 = 1;
pub const MAX_SET_WIDTH: u16 = 200;

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
    /// When set, sort by this slice of the field rather than the whole value.
    pub key: Option<SortKey>,
}

/// A width adjustment in progress (`r` for one column, `R` for the rest of the
/// row too). Widths change as the keys are pressed, so `Esc` needs the previous
/// ones to put back.
///
/// Adjusting gives every covered column the *same* width — the point of `R` is
/// to even out a block of columns, so nudging them by a relative amount would
/// leave them as uneven as they started. `%` is the exception: it fits each
/// column to its own values, after which they keep their own sizes and move
/// together.
#[derive(Clone)]
pub struct Resize {
    /// First display position covered.
    pub from: usize,
    /// How many positions are being resized.
    pub count: usize,
    /// The one width every covered column takes while adjusting together.
    target: u16,
    /// Set by `%`: the columns now differ, so keep their sizes and move them
    /// all by the same amount instead of flattening them again.
    relative: bool,
    /// What the covered columns' widths were before this started.
    saved: Vec<(usize, Option<u16>)>,
}

/// The `S` wizard: choosing which slice of a column to sort by, with the
/// choice drawn into every cell of that column so the same offsets can be
/// judged across the whole column at once.
#[derive(Clone, Copy)]
pub struct KeySort {
    /// The dataset column being sliced.
    pub col: usize,
    /// First character of the slice.
    pub start: usize,
    /// One past the last character; while picking the start it is `start + 1`.
    pub end: usize,
    pub stage: KeyStage,
    /// Longest value on screen in that column, so the edges can be clamped.
    pub width: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum KeyStage {
    /// Moving the left edge of the slice.
    Start,
    /// Moving the right edge.
    End,
    /// Choosing how to compare it.
    Method,
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
    /// Which *displayed* column is selected: an index into `cols`, not a
    /// dataset column — the two differ once columns are moved or hidden.
    pub selected_pos: usize,
    /// Dataset columns in display order. `x` drops one, `[`/`]` move them and
    /// `u` restores the lot; the data itself is never touched.
    cols: Vec<usize>,
    /// Rows of table body the last render could fit; used for paging.
    pub viewport_rows: usize,
    pub should_quit: bool,
    /// Set if a render pass failed; surfaced after the terminal is restored.
    pub render_error: Option<String>,

    pub mode: Mode,
    /// Buffer backing the prompt while in `Mode::Input`.
    pub input: String,
    /// Paths offered by `Tab` in the open prompt, while the picker is up.
    pub completions: Option<Completions>,
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
    /// Set when the selected row should become the header — or, when one has
    /// already been promoted, when that should be undone.
    pub promote_header: bool,
    /// Set when the user asks to start the join wizard.
    pub join_request: bool,
    /// Whether the loop is currently running the join wizard, so `Enter`
    /// confirms a pick and `Esc` backs out of the wizard before anything else.
    pub join_active: bool,
    /// Set by `Enter` while the wizard is running.
    pub confirm: bool,
    /// Set by `Esc` while the wizard is running.
    pub cancel_join: bool,
    /// The `S` sort-key wizard, while one is running.
    pub key_sort: Option<KeySort>,
    /// Widths the user has set, keyed by dataset column. A column without one
    /// is sized to its contents as usual.
    col_widths: HashMap<usize, u16>,
    /// The width adjustment in progress, if any.
    pub resize: Option<Resize>,
    /// Whether the `?` key reference is covering the table.
    pub show_help: bool,
    /// First line of the key reference on screen, for scrolling it.
    pub help_offset: usize,
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
        let cols = (0..data.ncols).collect();
        Self {
            data,
            filter: None,
            view: View::All,
            row_offset: 0,
            col_offset: 0,
            selected_row: 0,
            selected_pos: 0,
            cols,
            viewport_rows: 1,
            should_quit: false,
            render_error: None,
            mode: Mode::Normal,
            input: String::new(),
            completions: None,
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
            promote_header: false,
            join_request: false,
            join_active: false,
            confirm: false,
            cancel_join: false,
            key_sort: None,
            col_widths: HashMap::new(),
            resize: None,
            show_help: false,
            help_offset: 0,
            frozen_cols: 0,
            sort: None,
            num_styles: HashMap::new(),
            repeat: None,
        }
    }

    pub fn row_count(&self) -> usize {
        self.view.len(self.data.nrows)
    }

    /// The dataset columns on display, in order.
    pub fn visible_cols(&self) -> &[usize] {
        &self.cols
    }

    /// The dataset column under the cursor.
    pub fn selected_col(&self) -> usize {
        self.cols.get(self.selected_pos).copied().unwrap_or(0)
    }

    /// How many columns are hidden.
    pub fn hidden_count(&self) -> usize {
        self.data.ncols.saturating_sub(self.cols.len())
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
        self.cols.len().saturating_sub(1)
    }

    /// Move the selected column one place left or right, taking the cursor with
    /// it so it stays on the same column.
    fn shift_col(&mut self, delta: isize) {
        let to = self.selected_pos as isize + delta;
        if to < 0 || to > self.last_col() as isize {
            return;
        }
        let to = to as usize;
        self.cols.swap(self.selected_pos, to);
        self.selected_pos = to;
        self.status_msg = Some(format!(
            "moved {} to column {}",
            self.data.column_names[self.selected_col()],
            to + 1
        ));
    }

    /// Drop the selected column from the display. The last one stays: a table
    /// with no columns is nothing to look at.
    fn hide_col(&mut self) {
        if self.cols.len() < 2 {
            self.status_msg = Some("the last column stays".into());
            return;
        }
        let name = self.data.column_names[self.selected_col()].clone();
        self.cols.remove(self.selected_pos);
        self.selected_pos = self.selected_pos.min(self.last_col());
        self.status_msg = Some(format!("hid {name} · u restores"));
    }

    /// Put every column back: order, visibility and widths.
    fn restore_cols(&mut self) {
        let was = self.selected_col();
        self.cols = (0..self.data.ncols).collect();
        self.col_widths.clear();
        self.selected_pos = was.min(self.last_col());
        self.status_msg = Some("all columns restored".into());
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        self.handle_key_at(key, Instant::now());
    }

    /// `now` is injected so held-key acceleration can be tested deterministically.
    pub fn handle_key_at(&mut self, key: KeyEvent, now: Instant) {
        // The key reference swallows input while it is up, so a stray key can't
        // move the cursor behind it.
        if self.show_help {
            return self.handle_help(key);
        }
        if self.key_sort.is_some() {
            return self.handle_key_sort(key);
        }
        if self.resize.is_some() {
            return self.handle_resize(key);
        }
        match self.mode {
            Mode::Normal => self.handle_normal(key, now),
            Mode::Input(kind) => self.handle_input(key, kind),
        }
    }

    /// Scroll or dismiss the `?` key reference. Only `Ctrl-c` still quits.
    fn handle_help(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let scroll = |off: usize, by: usize| off.saturating_add(by);
        match key.code {
            KeyCode::Char('c') if ctrl => self.should_quit = true,
            KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter => {
                self.show_help = false;
                self.help_offset = 0;
            }
            KeyCode::Char('j') | KeyCode::Down => self.help_offset = scroll(self.help_offset, 1),
            KeyCode::Char('k') | KeyCode::Up => {
                self.help_offset = self.help_offset.saturating_sub(1)
            }
            KeyCode::PageDown | KeyCode::Char('f') if !ctrl => {
                self.help_offset = scroll(self.help_offset, 10)
            }
            KeyCode::PageUp | KeyCode::Char('b') => {
                self.help_offset = self.help_offset.saturating_sub(10)
            }
            KeyCode::Char('g') | KeyCode::Home => self.help_offset = 0,
            _ => {}
        }
    }

    /// Original row indices of the current view, capped at `max` (used to build
    /// the transposed table without unbounded width on large files).
    pub fn view_rows(&self, max: usize) -> Vec<usize> {
        (0..self.row_count().min(max)).map(|i| self.orig_row(i)).collect()
    }

    /// The width set for `col`, if the user has chosen one.
    pub fn col_width(&self, col: usize) -> Option<u16> {
        self.col_widths.get(&col).copied()
    }

    /// Begin adjusting widths: this column, or this one and all those right of
    /// it. The current widths are remembered so `Esc` can put them back.
    fn start_resize(&mut self, rest_of_row: bool) {
        if self.cols.is_empty() {
            return;
        }
        let from = self.selected_pos;
        let count = if rest_of_row {
            self.cols.len() - from
        } else {
            1
        };
        let saved = self.cols[from..from + count]
            .iter()
            .map(|&col| (col, self.col_width(col)))
            .collect();
        let selected = self.selected_col();
        let target = self
            .col_width(selected)
            .unwrap_or_else(|| self.natural_width(selected));
        self.resize = Some(Resize {
            from,
            count,
            target,
            relative: false,
            saved,
        });
        // `R` evens the block out at once, so what it does is visible before
        // anything is adjusted. `r` leaves its one column sizing itself until
        // asked otherwise.
        if count > 1 {
            self.set_uniform_width();
        }
    }

    /// Give every covered column the resize's single width.
    fn set_uniform_width(&mut self) {
        let Some(target) = self.resize.as_ref().map(|r| r.target) else {
            return;
        };
        for col in self.resizing() {
            self.col_widths.insert(col, target);
        }
    }

    /// The dataset columns a resize covers.
    fn resizing(&self) -> Vec<usize> {
        match &self.resize {
            Some(r) => self.cols[r.from..(r.from + r.count).min(self.cols.len())].to_vec(),
            None => Vec::new(),
        }
    }

    /// Adjust widths, put them back, or leave them be. Both the arrows and
    /// `h`/`l`/`j`/`k` work, since neither pair is obviously the right one for
    /// a width.
    fn handle_resize(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        match key.code {
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Down | KeyCode::Char('j') => {
                self.nudge_widths(-1)
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Up | KeyCode::Char('k') => {
                self.nudge_widths(1)
            }
            // Fit the values and ignore the column's name, which is often the
            // longer of the two — the status bar shows a clipped name anyway.
            KeyCode::Char('%') => self.fit_widths_to_values(),
            // Back to sizing by name and content together.
            KeyCode::Char('0') | KeyCode::Char('=') => {
                for col in self.resizing() {
                    self.col_widths.remove(&col);
                }
                if let Some(resize) = &mut self.resize {
                    resize.relative = false;
                }
                self.status_msg = Some("width: auto".into());
            }
            KeyCode::Enter => {
                self.resize = None;
                self.status_msg = Some("width kept".into());
            }
            KeyCode::Esc => {
                // Put back exactly what was there before.
                if let Some(resize) = self.resize.take() {
                    for (col, width) in resize.saved {
                        match width {
                            Some(w) => self.col_widths.insert(col, w),
                            None => self.col_widths.remove(&col),
                        };
                    }
                }
                self.status_msg = Some("width unchanged".into());
            }
            _ => {}
        }
    }

    /// Widen or narrow the covered columns by one.
    ///
    /// Normally they all end up at the same width: `R` is for evening out a
    /// block of columns, so a column narrower than the target gets *wider*.
    /// After `%` they hold their own sizes and shift together instead.
    fn nudge_widths(&mut self, delta: i16) {
        let clamp = |w: i16| w.clamp(MIN_SET_WIDTH as i16, MAX_SET_WIDTH as i16) as u16;
        let relative = self.resize.as_ref().is_some_and(|r| r.relative);
        if relative {
            for col in self.resizing() {
                // Without an explicit width the column sizes itself, so start
                // the nudge from whatever that came out as.
                let current = self
                    .col_width(col)
                    .unwrap_or_else(|| self.natural_width(col));
                let next = clamp(current as i16 + delta);
                self.col_widths.insert(col, next);
            }
            return;
        }
        if let Some(resize) = &mut self.resize {
            resize.target = clamp(resize.target as i16 + delta);
        }
        self.set_uniform_width();
    }

    /// Fit each covered column to its own values, ignoring the column name.
    /// With `R` that leaves the columns at different widths — each one is as
    /// wide as its data needs — which is the other thing you might want from a
    /// block of columns.
    fn fit_widths_to_values(&mut self) {
        for col in self.resizing() {
            let values = self.visible_width(col);
            // A column with nothing on screen keeps enough room for its `NA`s.
            let width = if values == 0 {
                MIN_COL_WIDTH
            } else {
                (values as u16).clamp(MIN_SET_WIDTH, MAX_SET_WIDTH)
            };
            self.col_widths.insert(col, width);
        }
        if let Some(resize) = &mut self.resize {
            resize.relative = true;
        }
        self.status_msg = Some("width: fitted to the values".into());
    }

    /// Roughly what a column sizes itself to: the widest of its name and the
    /// values on screen, within the usual cap.
    fn natural_width(&self, col: usize) -> u16 {
        let name = self.data.column_names[col].chars().count();
        let widest = self.visible_width(col);
        (name.max(widest) as u16).clamp(MIN_COL_WIDTH, MAX_COL_WIDTH)
    }

    /// Start the `S` wizard on the selected column.
    fn start_key_sort(&mut self) {
        if self.row_count() == 0 {
            return;
        }
        let col = self.selected_col();
        let width = self.visible_width(col);
        if width == 0 {
            self.status_msg = Some("nothing to slice in this column".into());
            return;
        }
        self.key_sort = Some(KeySort {
            col,
            start: 0,
            end: 1,
            stage: KeyStage::Start,
            width,
        });
    }

    /// The longest value on screen in `col`, which bounds the slice.
    fn visible_width(&self, col: usize) -> usize {
        let end = (self.row_offset + self.viewport_rows).min(self.row_count());
        let rows: Vec<usize> = (self.row_offset..end).map(|i| self.orig_row(i)).collect();
        self.data
            .cells(col, &rows)
            .map(|cells| {
                cells
                    .iter()
                    .flatten()
                    .map(|v| v.chars().count())
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0)
    }

    /// Drive the `S` wizard: the arrows (or `h`/`l`) move the edge being
    /// chosen, `j`/`k` scroll so more values can be judged against it, `Enter`
    /// moves on and `Esc` gives up.
    fn handle_key_sort(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        let Some(mut wizard) = self.key_sort else { return };
        let last = wizard.width.saturating_sub(1);
        match key.code {
            KeyCode::Esc => {
                self.key_sort = None;
                self.status_msg = Some("sort key cancelled".into());
                return;
            }
            KeyCode::Left | KeyCode::Char('h') => match wizard.stage {
                KeyStage::Start => {
                    wizard.start = wizard.start.saturating_sub(1);
                    wizard.end = wizard.start + 1;
                }
                // The right edge never crosses the left one.
                KeyStage::End => wizard.end = (wizard.end - 1).max(wizard.start + 1),
                KeyStage::Method => {}
            },
            KeyCode::Right | KeyCode::Char('l') => match wizard.stage {
                KeyStage::Start => {
                    wizard.start = (wizard.start + 1).min(last);
                    wizard.end = wizard.start + 1;
                }
                KeyStage::End => wizard.end = (wizard.end + 1).min(wizard.width),
                KeyStage::Method => {}
            },
            // Scrolling is allowed: the point is to judge the offsets against
            // the other values in the column.
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_row(1);
                self.key_sort = Some(wizard);
                return;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_row(-1);
                self.key_sort = Some(wizard);
                return;
            }
            KeyCode::Enter => match wizard.stage {
                // The end edge opens at the far side of the field, so taking
                // everything from the start onwards — the common case — needs no
                // moving at all: `S`, Enter, Enter, method.
                KeyStage::Start => {
                    wizard.stage = KeyStage::End;
                    wizard.end = wizard.width;
                }
                KeyStage::End | KeyStage::Method => wizard.stage = KeyStage::Method,
            },
            KeyCode::Char('a') if wizard.stage == KeyStage::Method => {
                return self.apply_key_sort(wizard, SortMethod::Alphabetic);
            }
            KeyCode::Char('n') if wizard.stage == KeyStage::Method => {
                return self.apply_key_sort(wizard, SortMethod::Numeric);
            }
            KeyCode::Char('v') if wizard.stage == KeyStage::Method => {
                return self.apply_key_sort(wizard, SortMethod::Natural);
            }
            _ => {}
        }
        self.key_sort = Some(wizard);
    }

    /// Sort by the chosen slice, ascending. `s` afterwards cycles the direction
    /// as usual, keeping the slice.
    fn apply_key_sort(&mut self, wizard: KeySort, method: SortMethod) {
        self.key_sort = None;
        let key = SortKey {
            start: wizard.start,
            end: wizard.end,
            method,
        };
        let previous = self.sort;
        self.sort = Some(SortSpec {
            col: wizard.col,
            dir: SortDir::Asc,
            key: Some(key),
        });
        if !self.rebuild_view() {
            interrupt::take();
            self.sort = previous;
            self.status_msg = Some("sort cancelled".into());
            return;
        }
        self.status_msg = Some(format!(
            "sorted ↑ {}[{}-{}] {}",
            self.data.column_names[wizard.col],
            wizard.start + 1,
            wizard.end,
            method.label(),
        ));
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
                if self.join_active {
                    self.cancel_join = true;
                } else if self.search.take().is_some() {
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
            KeyCode::Char('?') => self.show_help = true,
            // `J` starts the join wizard; `Enter` then picks the key column
            // under the cursor, once on each side.
            KeyCode::Char('J') => self.join_request = true,
            KeyCode::Enter if self.join_active => self.confirm = true,
            // `T` re-reads the file with the first row as names or as data;
            // `H` moves the header down to the selected row (or undoes that).
            KeyCode::Char('T') => {
                if self.header_applies() {
                    self.toggle_header = true;
                }
            }
            KeyCode::Char('H') => {
                if self.header_applies() {
                    self.promote_header = true;
                }
            }
            KeyCode::Char('%') => self.toggle_numeric(),
            KeyCode::Char('>') => self.adjust_decimals(1),
            KeyCode::Char('<') => self.adjust_decimals(-1),
            KeyCode::Char('s') => self.cycle_sort(),
            KeyCode::Char('S') => self.start_key_sort(),
            // `r` adjusts this column's width, `R` this one and every column
            // to its right.
            KeyCode::Char('r') => self.start_resize(false),
            KeyCode::Char('R') => self.start_resize(true),
            KeyCode::Char('f') if !ctrl => self.toggle_freeze(),
            KeyCode::Char('/') => self.enter_input(InputKind::Search),
            // Column-scoped search. `-` is a direct, unshifted key on AZERTY;
            // change this char to rebind.
            KeyCode::Char('-') => self.enter_input(InputKind::ColumnSearch),
            KeyCode::Char('&') => self.enter_input(InputKind::Filter),
            KeyCode::Char(':') => self.enter_input(InputKind::Goto),
            KeyCode::Char('n') => self.jump_match(true),
            KeyCode::Char('N') => self.jump_match(false),

            // Columns: `x` hides, `u` restores, `[`/`]` (or Shift-arrows) move.
            KeyCode::Char('x') => self.hide_col(),
            KeyCode::Char('u') if !ctrl => self.restore_cols(),
            KeyCode::Char('[') => self.shift_col(-1),
            KeyCode::Char(']') => self.shift_col(1),
            KeyCode::Left if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.shift_col(-1)
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.shift_col(1)
            }

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
                self.selected_pos = 0;
                self.col_offset = 0;
            }
            KeyCode::Char('$') => self.selected_pos = self.last_col(),
            _ => {}
        }
    }

    /// Whether a header change makes sense here: a transposed view is built
    /// from the table, not read from the file, so it has no header row to move.
    fn header_applies(&mut self) -> bool {
        if self.is_transposed {
            self.status_msg = Some("header applies to the file — press t first".into());
            return false;
        }
        true
    }

    fn handle_input(&mut self, key: KeyEvent, kind: InputKind) {
        // The open prompt browses the filesystem; the others are plain text.
        if let InputKind::Open = kind
            && self.handle_open_key(key)
        {
            return;
        }
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.input.clear();
                self.completions = None;
            }
            KeyCode::Enter => self.commit_input(kind),
            KeyCode::Backspace => {
                self.input.pop();
                self.refresh_completions();
            }
            // Goto only accepts digits; search/filter take any character.
            KeyCode::Char(c) => {
                if !matches!(kind, InputKind::Goto) || c.is_ascii_digit() {
                    self.input.push(c);
                    self.refresh_completions();
                }
            }
            _ => {}
        }
    }

    /// Keys specific to the open prompt. Returns `true` when the key was one of
    /// them, so ordinary typing falls through to the shared handler.
    fn handle_open_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            // First `Tab` lists the folder (completing as far as it can);
            // afterwards it walks the list.
            KeyCode::Tab => match self.completions.is_some() {
                false => self.open_completions(true),
                true => self.step_completion(1),
            },
            KeyCode::BackTab => self.step_completion(-1),
            KeyCode::Down => self.step_completion(1),
            KeyCode::Up => self.step_completion(-1),
            KeyCode::Enter if self.completions.is_some() => self.accept_completion(),
            // `Esc` puts the picker away first, leaving what was typed.
            KeyCode::Esc if self.completions.is_some() => self.completions = None,
            _ => return false,
        }
        true
    }

    /// The folder a bare name is taken against: where the file on screen lives.
    pub fn base_dir(&self) -> std::path::PathBuf {
        self.data
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("."))
    }

    /// List the folder the typed path points into.
    ///
    /// `complete` is what `Tab` does on top of listing: extend the input by
    /// whatever every candidate agrees on, and finish outright when a single
    /// file matches. Stepping into a folder and typing both pass `false` — there
    /// the listing is the point, and finishing the path under someone's fingers
    /// would swallow the characters they type next.
    fn open_completions(&mut self, complete: bool) {
        let listing = Completions::for_input(&self.input, &self.base_dir());
        if complete {
            if let Some(common) = listing.common_prefix() {
                self.input = common;
            }
            if listing.entries.len() == 1 && !listing.entries[0].is_dir {
                self.completions = None;
                return;
            }
        }
        self.completions = Some(listing);
    }

    fn step_completion(&mut self, delta: isize) {
        if let Some(listing) = &mut self.completions {
            listing.step(delta);
        }
    }

    /// Take the highlighted entry: step into a directory, or open a file.
    fn accept_completion(&mut self) {
        let Some(listing) = &self.completions else { return };
        let Some(entry_is_dir) = listing.selected_entry().map(|e| e.is_dir) else {
            return;
        };
        let Some(text) = listing.selected_input() else { return };
        self.input = text;
        if entry_is_dir {
            self.open_completions(false); // list what is inside it
        } else {
            self.completions = None;
            self.commit_input(InputKind::Open);
        }
    }

    /// Re-list after the typed path changed, while the picker is up.
    fn refresh_completions(&mut self) {
        if self.completions.is_some() {
            self.completions = None;
            // Still typing: narrow the list, never finish the path for them.
            self.open_completions(false);
        }
    }

    fn enter_input(&mut self, kind: InputKind) {
        self.mode = Mode::Input(kind);
        self.status_msg = None;
        self.completions = None;
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
        self.completions = None;
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
            InputKind::ColumnSearch => self.apply_search(query, re, Some(self.selected_col())),
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
        match self.data.filter_rows(self.visible_cols(), &re, interrupt::requested) {
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
        self.cycle_sort_col(self.selected_col());
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
        // Cycling the direction of a keyed sort keeps its slice.
        let key = self.sort.filter(|s| s.col == col).and_then(|s| s.key);
        self.sort = next.map(|dir| SortSpec { col, dir, key });
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
        self.toggle_numeric_col(self.selected_col());
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
        self.adjust_decimals_col(self.selected_col(), delta);
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
        let want = self.selected_pos + 1;
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
                let descending = s.dir == SortDir::Desc;
                let ordered = match s.key {
                    Some(key) => self.data.sort_indices_by_key(
                        &base,
                        s.col,
                        key,
                        descending,
                        interrupt::requested,
                    ),
                    None => {
                        self.data
                            .sort_indices(&base, s.col, descending, interrupt::requested)
                    }
                };
                match ordered {
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
        // Searching walks the columns on display, so a hit can never land the
        // cursor on a column that has been hidden.
        let found = self.data.find_match(
            &search.re,
            self.row_count(),
            self.visible_cols(),
            |i| self.view.orig(i),
            self.selected_row,
            self.selected_pos,
            forward,
            search.scope,
            interrupt::requested,
        );
        match found {
            Some((row, pos)) => {
                self.selected_row = row;
                self.selected_pos = pos;
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
        self.selected_pos =
            (self.selected_pos as isize + delta).clamp(0, self.last_col() as isize) as usize;
    }
}

/// Compile a user query as a case-insensitive regex (csvlens-style search).
fn build_regex(query: &str) -> Result<Regex, regex::Error> {
    RegexBuilder::new(query).case_insensitive(true).build()
}

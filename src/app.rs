use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use regex::{Regex, RegexBuilder};

use crate::browse::Completions;
use crate::data::{ColumnStats, Dataset, Derived, HeaderSpec, Recipe, SortKey, SortMethod};
use crate::formula::FormulaError;
use crate::pattern::{
    Pattern, SavedColumn, SavedHeader, SavedNumStyle, SavedSort, SavedSortKey,
};
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
    /// Name the file (or glob) a saved pattern is tied to.
    Pattern,
    /// The pattern or formula a new column is worked out with.
    Recipe,
    /// What to call a column: a new one, or one being renamed.
    ColumnName,
    /// Where to write this view out to.
    Export,
}

/// The rectangle a crop covers: a run of view rows and a run of columns, both
/// as inclusive pairs in the order they are shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CropSpan {
    pub rows: (usize, usize),
    /// Display positions, not dataset columns: what is taken is what is on
    /// screen between the two corners, so hidden columns are already out of it.
    pub cols: (usize, usize),
}

/// Where a tab came from, for the tabs that were made here rather than read
/// from a file. Kept so a session can describe one and make it again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    Join(JoinOrigin),
    Crop(CropOrigin),
}

/// The two sides a joined view was made from: which tab, and which column of it
/// held the key. Tabs are held by index, which the loop keeps straight as tabs
/// come and go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JoinOrigin {
    pub left_tab: usize,
    pub right_tab: usize,
    pub left_key: String,
    pub right_key: String,
}

/// A run of rows taken from another tab (`c`), described by where it started
/// and ended rather than by which rows they were.
///
/// Row numbers would not survive a reload: the same run is found again by
/// looking for the two values that bounded it in the column the tab was sorted
/// by, which is the column that made the run a run in the first place. An
/// unsorted tab is ordered by its row numbers, and then those are the bounds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CropOrigin {
    pub tab: usize,
    /// The column the bounds are read in, or empty for the row-number gutter.
    pub column: String,
    pub from: String,
    pub to: String,
    /// How many rows it held when it was taken, as a check on the way back: a
    /// boundary value shared by several rows takes all of them in, so a crop
    /// can come back taller than it went out.
    pub rows: usize,
    /// The columns kept, by name and in the order they were shown. The column
    /// half of a crop needs no locating: a name is a name.
    pub columns: Vec<String>,
}

/// Something the loop is waiting for a yes or a no about.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Question {
    /// Quitting with a session that has changed since it was saved.
    LoseSession,
    /// Writing over a file that is already there.
    Overwrite(std::path::PathBuf),
}

/// Adding a column (`a`), one step at a time. The source column is the one
/// under the cursor when it starts, so a regex extraction needs no more than
/// the pattern itself.
#[derive(Clone)]
pub struct NewColumn {
    /// `None` until the kind has been chosen.
    pub kind: Option<NewKind>,
    /// The column the cursor was on, which an extraction reads.
    pub source: usize,
    /// The pattern or formula, once typed.
    pub recipe: Option<String>,
}

/// Something to say that will not fit on the status line, shown in the middle
/// of the screen. A formula that would not do brings the text it came from, so
/// the trouble can be pointed at rather than described; anything else is just
/// the words.
#[derive(Clone)]
pub struct Notice {
    pub title: &'static str,
    /// The text being complained about — a formula — when there is one.
    pub subject: Option<String>,
    /// Character offset of the trouble within `subject`.
    pub at: Option<usize>,
    pub message: String,
    pub hint: Option<String>,
    /// Whether a keypress puts it away. A complaint about a formula stays until
    /// the formula is edited, since the prompt it belongs to is still up; a
    /// question stays until it is answered.
    pub dismissable: bool,
    /// What the bottom of the box says the keys are.
    pub footer: &'static str,
}

impl Notice {
    /// A plain complaint, put away by the next keypress.
    pub fn say(title: &'static str, message: impl Into<String>) -> Self {
        Self {
            title,
            subject: None,
            at: None,
            message: message.into(),
            hint: None,
            dismissable: true,
            footer: " any key ",
        }
    }

    /// Something to answer rather than dismiss.
    pub fn ask(title: &'static str, message: impl Into<String>, keys: &'static str) -> Self {
        Self {
            title,
            subject: None,
            at: None,
            message: message.into(),
            hint: None,
            dismissable: false,
            footer: keys,
        }
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NewKind {
    /// A capture from the selected column's text.
    Extract,
    /// An expression over the columns.
    Formula,
}

/// How many steps `z` can walk back, and how many row indices the whole history
/// may hold. Views are shared, so the usual cost of a step is a handful of
/// pointers; the row budget is what stops a run of filters or sorts over a huge
/// file from pinning every intermediate row list in memory.
pub const MAX_UNDO_STEPS: usize = 32;
const MAX_UNDO_ROWS: usize = 8_000_000;

/// Everything `z` puts back: the view, how it is drawn, and where the cursor
/// was. Cheap to clone — the row lists inside are shared.
#[derive(Clone)]
struct ViewState {
    filter: Option<Arc<Vec<usize>>>,
    filter_query: Option<String>,
    view: View,
    sort: Option<SortSpec>,
    search: Option<Search>,
    cols: Vec<usize>,
    col_widths: HashMap<usize, u16>,
    num_styles: HashMap<usize, NumStyle>,
    /// The computed columns and renames, which live on the dataset rather than
    /// here — so undoing `a` or `R` means putting these back too.
    derived: Derived,
    frozen_cols: usize,
    selected_row: usize,
    selected_pos: usize,
    row_offset: usize,
    col_offset: usize,
}

impl ViewState {
    /// Row indices this state holds on to, for the history's budget.
    fn rows_held(&self) -> usize {
        match &self.view {
            View::All => 0,
            View::Rows(rows) => rows.len(),
        }
    }
}

/// One step back: what the view was, and the name of the change that left it.
struct Step {
    /// What was about to happen, so undo can say what it undid.
    label: &'static str,
    state: ViewState,
}

/// An active search: the raw query, its compiled regex, and its scope.
#[derive(Clone)]
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

/// Which columns the next column command applies to, set by `(` or `)`.
///
/// A scoped command works out what the *selected* column should become and
/// gives every covered column the same — the point of asking for a block is to
/// even it out, not to nudge each column from wherever it happened to be.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    /// The selected column and every column to its right.
    Rightward,
    /// The selected column and every column to its left.
    Leftward,
}

/// What the summary line shows. `Auto` is what the line opens as: a total,
/// except where a column is being read on a log scale (`%`), for which a total
/// rarely means anything and an average does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Summary {
    Auto,
    Total,
    Mean,
    Stddev,
    MeanStddev,
}

impl Summary {
    /// The order `=` walks through.
    fn next(self) -> Self {
        match self {
            Summary::Auto => Summary::Total,
            Summary::Total => Summary::Mean,
            Summary::Mean => Summary::Stddev,
            Summary::Stddev => Summary::MeanStddev,
            Summary::MeanStddev => Summary::Auto,
        }
    }

    /// Short marker for the gutter, where the row number would be.
    pub fn marker(self) -> &'static str {
        match self {
            Summary::Auto => "Σμ",
            Summary::Total => "Σ",
            Summary::Mean => "μ",
            Summary::Stddev => "σ",
            Summary::MeanStddev => "±",
        }
    }

    /// What the status bar calls it.
    pub fn label(self) -> &'static str {
        match self {
            Summary::Auto => "auto",
            Summary::Total => "total",
            Summary::Mean => "mean",
            Summary::Stddev => "sd",
            Summary::MeanStddev => "mean±sd",
        }
    }

    /// The name a pattern stores, and the one it reads back.
    pub fn name(self) -> &'static str {
        match self {
            Summary::Auto => "auto",
            Summary::Total => "total",
            Summary::Mean => "mean",
            Summary::Stddev => "sd",
            Summary::MeanStddev => "mean-sd",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "auto" => Summary::Auto,
            "total" => Summary::Total,
            "mean" => Summary::Mean,
            "sd" => Summary::Stddev,
            "mean-sd" => Summary::MeanStddev,
            _ => return None,
        })
    }
}

/// How many repeats of `=` count as holding it down, which puts the line away.
/// Key auto-repeat fires far faster than anyone presses deliberately, so a run
/// this long inside [`REPEAT_WINDOW`] means the key is down.
const SUMMARY_HOLD: usize = 4;
/// Identifies `=` in the held-key tracker, so it doesn't disturb scrolling.
const SUMMARY_REPEAT_ID: u8 = 20;

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
#[derive(Clone)]
enum View {
    All,
    /// Shared, so remembering a view for undo costs a pointer rather than a
    /// copy of every row index.
    Rows(Arc<Vec<usize>>),
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
    filter: Option<Arc<Vec<usize>>>,
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
    /// A binding typed at the `w` prompt: save this view's pattern under it, or
    /// forget the pattern when the text is empty. Handled by the loop, which
    /// owns the store.
    pub save_pattern: Option<String>,
    /// A name typed at the `X` prompt: write this view out there.
    pub export_request: Option<String>,
    /// Set when writing over a file that is already there has been agreed to.
    pub overwrite_allowed: bool,
    /// Set by `W`: remember every tab open here. The loop owns the tabs, so it
    /// is the only thing that can.
    pub save_session: bool,
    /// Where a joined view came from: the tabs and key columns it was made
    /// from. A join keeps no data of its own, so this is what lets a session
    /// make it again.
    pub origin: Option<Origin>,
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
    /// Adding a column, while that is going on.
    pub new_column: Option<NewColumn>,
    /// Something being said in the middle of the screen.
    pub notice: Option<Notice>,
    /// What the loop is waiting for an answer about.
    pub question: Option<Question>,
    /// Set once quitting has been answered, so it is not questioned twice.
    pub quit_anyway: bool,
    /// The `S` sort-key wizard, while one is running.
    pub key_sort: Option<KeySort>,
    /// The first corner of a crop — a view row and a display position — while
    /// `c` waits for the opposite one.
    pub crop_mark: Option<(usize, usize)>,
    /// A crop asked for: the rectangle its two corners enclose.
    pub crop_request: Option<CropSpan>,
    /// Widths the user has set, keyed by dataset column. A column without one
    /// is sized to its contents as usual.
    col_widths: HashMap<usize, u16>,
    /// The width adjustment in progress, if any.
    pub resize: Option<Resize>,
    /// A pending `(`/`)`: which columns the next column command covers.
    pub scope: Option<Scope>,
    /// The folder lambris was started in, which is where a session belongs and
    /// where `X` writes when given a bare name. Kept in step by the loop, the
    /// way `join_active` is.
    pub folder: std::path::PathBuf,
    /// Whether the summary line is on, and what a column shows unless it says
    /// otherwise.
    pub summary: Option<Summary>,
    /// Columns showing something other than the default.
    summary_cols: HashMap<usize, Summary>,
    /// Totals worked out per dataset column for the rows on display. Kept until
    /// the set of rows changes, so cycling `=` costs nothing.
    stats: HashMap<usize, ColumnStats>,
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
    /// Views to go back to, oldest first.
    undo: Vec<Step>,
    /// Views to go forward to, cleared as soon as anything new happens.
    redo: Vec<Step>,
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
            save_pattern: None,
            export_request: None,
            overwrite_allowed: false,
            save_session: false,
            origin: None,
            toggle_header: false,
            promote_header: false,
            join_request: false,
            join_active: false,
            confirm: false,
            cancel_join: false,
            new_column: None,
            notice: None,
            question: None,
            quit_anyway: false,
            key_sort: None,
            crop_mark: None,
            crop_request: None,
            col_widths: HashMap::new(),
            resize: None,
            scope: None,
            folder: std::path::PathBuf::from("."),
            summary: None,
            summary_cols: HashMap::new(),
            stats: HashMap::new(),
            show_help: false,
            help_offset: 0,
            frozen_cols: 0,
            sort: None,
            num_styles: HashMap::new(),
            repeat: None,
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    /// What a pattern for this view would be tied to by default: the file's
    /// name, which follows the file if it is regenerated elsewhere.
    pub fn pattern_bind(&self) -> String {
        self.data
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.data.path.display().to_string())
    }

    /// Whether a pattern can be tied to this view at all: it has to be a file's
    /// own content, which a join or a transposed view is not.
    pub fn is_file_backed(&self) -> bool {
        self.data.is_file_backed()
    }

    /// Describe this view as a pattern tied to `bind`.
    ///
    /// Everything is written out by column *name*, worked out from the live
    /// view at the moment of asking — there is nothing to keep in step as the
    /// view changes, and nothing to drift.
    pub fn pattern(&self, bind: String) -> Pattern {
        let name = |col: usize| self.data.column_names[col].clone();
        let shown: Vec<String> = self.cols.iter().map(|&c| name(c)).collect();
        let hidden: Vec<String> = (0..self.data.ncols)
            .filter(|c| !self.cols.contains(c))
            .map(name)
            .collect();
        Pattern {
            bind,
            sheet: self.data.sheet().map(str::to_string),
            columns: shown,
            hidden,
            widths: self
                .col_widths
                .iter()
                .map(|(&col, &w)| (name(col), w))
                .collect(),
            numeric: self
                .num_styles
                .iter()
                .map(|(&col, style)| {
                    (
                        name(col),
                        SavedNumStyle {
                            align: style.align,
                            log: style.log,
                            decimals: style.decimals,
                        },
                    )
                })
                .collect(),
            sort: self.sort.map(|s| SavedSort {
                column: name(s.col),
                descending: s.dir == SortDir::Desc,
                key: s.key.map(|k| SavedSortKey {
                    // Written the way `sort -k` writes them: 1-based, inclusive.
                    from: k.start + 1,
                    to: k.end,
                    method: k.method.label().to_string(),
                }),
            }),
            // The frozen prefix is named by its last column, so it survives a
            // column before it going missing.
            frozen_through: (self.frozen_cols > 0)
                .then(|| self.cols.get(self.frozen_cols - 1).map(|&c| name(c)))
                .flatten(),
            filter: self.filter_query.clone(),
            // `Auto` is the absence of a stated reading, so it is left out —
            // a pattern should not pin down what the file already says.
            header: match self.data.header {
                HeaderSpec::Auto => None,
                HeaderSpec::At { skip, named } => Some(SavedHeader { skip, named }),
            },
            row_numbers: self.show_line_numbers,
            computed: self
                .data
                .derived()
                .recipes()
                .into_iter()
                .map(|(name, recipe)| match recipe {
                    Recipe::Extract { source, pattern } => SavedColumn {
                        name,
                        from: Some(source),
                        extract: Some(pattern),
                        formula: None,
                    },
                    Recipe::Formula { expression } => SavedColumn {
                        name,
                        from: None,
                        extract: None,
                        formula: Some(expression),
                    },
                })
                .collect(),
            renamed: self
                .data
                .derived()
                .renames_by_original(&self.data.original_names)
                .into_iter()
                .collect(),
            summary: self.summary.map(|s| s.name().to_string()),
            summaries: self
                .summary_cols
                .iter()
                .filter(|(_, mode)| Some(**mode) != self.summary)
                .map(|(&col, mode)| (name(col), mode.name().to_string()))
                .collect(),
        }
    }

    /// Arrange this view as `pattern` describes.
    ///
    /// Names that the file no longer has are skipped, and columns the file has
    /// gained — in neither list — stay visible at the end rather than vanishing
    /// because a pattern written before them did not mention them.
    pub fn apply_pattern(&mut self, pattern: &Pattern) {
        // The computed columns and renames come first: everything after this
        // refers to columns by name, including the ones made here.
        for saved in &pattern.computed {
            let recipe = match (&saved.from, &saved.extract, &saved.formula) {
                (Some(from), Some(extract), _) => Recipe::Extract {
                    source: from.clone(),
                    pattern: extract.clone(),
                },
                (_, _, Some(formula)) => Recipe::Formula {
                    expression: formula.clone(),
                },
                _ => continue,
            };
            // A recipe naming a column the file no longer has is skipped; the
            // rest of the arrangement still applies.
            let _ = self.data.add_computed(&saved.name, recipe);
        }
        for (was, now) in &pattern.renamed {
            if let Some(col) = self.data.original_names.iter().position(|n| n == was) {
                let _ = self.data.rename(col, now);
            }
        }
        self.cols = (0..self.data.ncols).collect();
        self.selected_pos = 0;

        let index_of = |wanted: &str| {
            self.data
                .column_names
                .iter()
                .position(|name| name == wanted)
        };

        let mut ordered: Vec<usize> = pattern.columns.iter().filter_map(|n| index_of(n)).collect();
        let known: Vec<usize> = pattern
            .columns
            .iter()
            .chain(&pattern.hidden)
            .filter_map(|n| index_of(n))
            .collect();
        ordered.extend((0..self.data.ncols).filter(|c| !known.contains(c)));
        if !ordered.is_empty() {
            self.cols = ordered;
        }

        self.col_widths = pattern
            .widths
            .iter()
            .filter_map(|(name, &w)| index_of(name).map(|c| (c, w)))
            .collect();
        self.num_styles = pattern
            .numeric
            .iter()
            .filter_map(|(name, style)| {
                index_of(name).map(|c| {
                    (
                        c,
                        NumStyle {
                            align: style.align,
                            log: style.log,
                            decimals: style.decimals,
                        },
                    )
                })
            })
            .collect();
        self.frozen_cols = pattern
            .frozen_through
            .as_deref()
            .and_then(index_of)
            .and_then(|col| self.cols.iter().position(|&c| c == col))
            .map(|pos| pos + 1)
            .unwrap_or(0);
        self.show_line_numbers = pattern.row_numbers;

        // A filter has to be run again — only the regex that made it is saved.
        if let Some(query) = &pattern.filter
            && let Ok(re) = build_regex(query)
        {
            self.filter_query = Some(query.to_string());
            if let Ok(Some(rows)) =
                self.data
                    .filter_rows(self.visible_cols(), &re, interrupt::requested)
            {
                self.filter = Some(Arc::new(rows));
            } else {
                interrupt::take();
                self.filter_query = None;
            }
        }
        if let Some(saved) = &pattern.sort
            && let Some(col) = index_of(&saved.column)
        {
            let method = |name: &str| match name {
                "num" => SortMethod::Numeric,
                "nat" => SortMethod::Natural,
                _ => SortMethod::Alphabetic,
            };
            self.sort = Some(SortSpec {
                col,
                dir: if saved.descending {
                    SortDir::Desc
                } else {
                    SortDir::Asc
                },
                key: saved.key.as_ref().map(|k| SortKey {
                    start: k.from.saturating_sub(1),
                    end: k.to.max(k.from),
                    method: method(&k.method),
                }),
            });
        }
        let default = pattern.summary.as_deref().and_then(Summary::from_name);
        // Worked out here, while the name lookup is still in hand.
        let modes: HashMap<usize, Summary> = pattern
            .summaries
            .iter()
            .filter_map(|(name, mode)| index_of(name).zip(Summary::from_name(mode)))
            .collect();
        self.summary = default;
        self.summary_cols = modes;
        if !self.rebuild_view() {
            // Interrupted while sorting: keep the arrangement, drop the sort.
            interrupt::take();
            self.sort = None;
            self.rebuild_view();
        }
        self.invalidate_stats();
        // A pattern is the starting point, not a change to walk back from.
        self.undo.clear();
        self.redo.clear();
    }

    /// The header reading a pattern asks for, which has to be known before the
    /// file is read rather than applied afterwards.
    pub fn header_from(pattern: &Pattern) -> Option<HeaderSpec> {
        pattern.header.map(|h| HeaderSpec::At {
            skip: h.skip,
            named: h.named,
        })
    }

    /// Take a copy of everything `z` can restore.
    fn snapshot(&self) -> ViewState {
        ViewState {
            filter: self.filter.clone(),
            filter_query: self.filter_query.clone(),
            view: self.view.clone(),
            sort: self.sort,
            search: self.search.clone(),
            cols: self.cols.clone(),
            col_widths: self.col_widths.clone(),
            num_styles: self.num_styles.clone(),
            derived: self.data.derived().clone(),
            frozen_cols: self.frozen_cols,
            selected_row: self.selected_row,
            selected_pos: self.selected_pos,
            row_offset: self.row_offset,
            col_offset: self.col_offset,
        }
    }

    /// Put a remembered view back, clamped in case the data no longer reaches
    /// that far.
    fn restore(&mut self, state: ViewState) {
        self.filter = state.filter;
        self.filter_query = state.filter_query;
        self.view = state.view;
        self.sort = state.sort;
        self.search = state.search;
        self.cols = state.cols;
        self.col_widths = state.col_widths;
        self.num_styles = state.num_styles;
        // Before the cursor is clamped: this decides how many columns there are.
        self.data.set_derived(state.derived);
        self.frozen_cols = state.frozen_cols;
        self.selected_row = state.selected_row.min(self.last_row());
        self.selected_pos = state.selected_pos.min(self.last_col());
        self.row_offset = state.row_offset;
        self.col_offset = state.col_offset;
        // A remembered view may have counted a different set of rows.
        self.invalidate_stats();
    }

    /// Remember the current view before changing it. `label` names the change
    /// that is about to happen, so `z` can say what it undid.
    ///
    /// Anything newly done invalidates the forward history, as everywhere else.
    fn record(&mut self, label: &'static str) {
        self.undo.push(Step {
            label,
            state: self.snapshot(),
        });
        self.redo.clear();
        // Oldest steps go first, by count and by how many rows they pin.
        let mut held: usize = self.undo.iter().map(|s| s.state.rows_held()).sum();
        while self.undo.len() > MAX_UNDO_STEPS
            || (held > MAX_UNDO_ROWS && self.undo.len() > 1)
        {
            let dropped = self.undo.remove(0);
            held = held.saturating_sub(dropped.state.rows_held());
        }
    }

    /// Drop the last recorded step, for a change that turned out not to happen
    /// (cancelled, or reverted by the operation itself).
    fn discard_record(&mut self) {
        self.undo.pop();
    }

    /// Step back to the view before the last change.
    fn undo(&mut self) {
        let Some(step) = self.undo.pop() else {
            self.status_msg = Some("nothing to undo".into());
            return;
        };
        let current = self.snapshot();
        self.restore(step.state);
        self.redo.push(Step {
            label: step.label,
            state: current,
        });
        self.status_msg = Some(format!("undid {}", step.label));
    }

    /// Step forward again.
    fn redo(&mut self) {
        let Some(step) = self.redo.pop() else {
            self.status_msg = Some("nothing to redo".into());
            return;
        };
        let current = self.snapshot();
        self.restore(step.state);
        self.undo.push(Step {
            label: step.label,
            state: current,
        });
        self.status_msg = Some(format!("redid {}", step.label));
    }

    pub fn row_count(&self) -> usize {
        self.view.len(self.data.nrows)
    }

    /// The dataset columns on display, in order.
    pub fn visible_cols(&self) -> &[usize] {
        &self.cols
    }

    /// The display positions a column command covers: just the selected one,
    /// or the block a pending `(`/`)` asked for.
    pub fn scoped_span(&self) -> (usize, usize) {
        let pos = self.selected_pos.min(self.last_col());
        match self.scope {
            Some(Scope::Rightward) => (pos, self.last_col()),
            Some(Scope::Leftward) => (0, pos),
            None => (pos, pos),
        }
    }

    /// The dataset columns a column command covers, in display order.
    fn scoped_cols(&self) -> Vec<usize> {
        let (from, to) = self.scoped_span();
        self.cols.get(from..=to).unwrap_or_default().to_vec()
    }

    /// How many columns that is, for a message.
    fn scoped_count(&self) -> usize {
        let (from, to) = self.scoped_span();
        to.saturating_sub(from) + 1
    }

    /// Note the block a command applied to, when it was more than one column.
    fn note_scope(&mut self, what: &str) {
        let count = self.scoped_count();
        self.status_msg = Some(if count > 1 {
            format!("{what} · {count} columns")
        } else {
            what.to_string()
        });
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
        self.record("column move");
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
        let going = self.scoped_cols();
        // Never hide everything: a table with no columns is nothing to look at,
        // so one stays however wide the scope was.
        let all_of_them = going.len() >= self.cols.len();
        let name = self.data.column_names[self.selected_col()].clone();
        let (from, _) = self.scoped_span();
        self.record("hide column");
        self.cols.retain(|c| !going.contains(c));
        if self.cols.is_empty() {
            self.cols = vec![going[0]];
        }
        self.selected_pos = from.min(self.last_col());
        let count = going.len();
        self.status_msg = Some(if count > 1 {
            let kept = if all_of_them { " (one kept)" } else { "" };
            format!("hid {count} columns{kept} · u restores")
        } else {
            format!("hid {name} · u restores")
        });
    }

    /// Put every column back: order, visibility and widths. Columns coming back
    /// into view may need totals of their own.
    fn restore_cols(&mut self) {
        self.record("restore columns");
        let was = self.selected_col();
        self.cols = (0..self.data.ncols).collect();
        self.col_widths.clear();
        self.selected_pos = was.min(self.last_col());
        self.refresh_stats();
        self.status_msg = Some("all columns restored".into());
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        self.handle_key_at(key, Instant::now());
    }

    /// `now` is injected so held-key acceleration can be tested deterministically.
    pub fn handle_key_at(&mut self, key: KeyEvent, now: Instant) {
        // The key reference swallows input while it is up, so a stray key can't
        // move the cursor behind it.
        if self.question.is_some() {
            return self.answer(key);
        }
        // Something said in the middle of the screen is read, then dismissed by
        // the next key — which does nothing else, so nothing happens unseen
        // behind it.
        if matches!(&self.notice, Some(n) if n.dismissable) {
            self.notice = None;
            return;
        }
        if self.show_help {
            return self.handle_help(key);
        }
        if self.key_sort.is_some() {
            return self.handle_key_sort(key);
        }
        if self.resize.is_some() {
            return self.handle_resize(key);
        }
        // Only while the kind is being chosen: the two prompts after that are
        // ordinary input.
        if matches!(&self.new_column, Some(new) if new.kind.is_none()) {
            return self.handle_new_column_kind(key);
        }
        match self.mode {
            Mode::Normal => self.handle_normal(key, now),
            Mode::Input(kind) => self.handle_input(key, kind),
        }
    }

    /// Answer a yes-or-no question. Anything other than the three answers is
    /// ignored: a question is asked where something stands to be lost, so a
    /// stray key must not decide it.
    fn answer(&mut self, key: KeyEvent) {
        let Some(question) = self.question.clone() else {
            return;
        };
        let yes = matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y'));
        let no = matches!(key.code, KeyCode::Char('n') | KeyCode::Char('N'));
        let back = matches!(key.code, KeyCode::Esc);
        if !(yes || no || back) {
            return;
        }
        match question {
            Question::LoseSession => {
                if yes {
                    self.save_session = true;
                }
                if yes || no {
                    self.should_quit = true;
                    self.quit_anyway = true;
                }
            }
            Question::Overwrite(path) => {
                if yes {
                    self.export_request = Some(path.to_string_lossy().into_owned());
                    self.overwrite_allowed = true;
                }
            }
        }
        self.question = None;
        self.notice = None;
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

    /// Mark an edge of a crop, or close one that is already marked.
    fn mark_crop(&mut self) {
        if self.join_active || self.row_count() == 0 {
            return;
        }
        let here = (self.selected_row, self.selected_pos);
        match self.crop_mark.take() {
            None => {
                self.crop_mark = Some(here);
                self.status_msg = None;
            }
            // Two corners, in either order. The same cell twice is a crop of
            // one cell, which is a reasonable way to say "just this".
            Some((row, pos)) => {
                let span = |a: usize, b: usize| (a.min(b), a.max(b));
                self.crop_request = Some(CropSpan {
                    rows: span(row, here.0),
                    cols: span(pos, here.1),
                });
            }
        }
    }

    /// How a crop of these view rows is written down: the column this tab is
    /// ordered by, and the values at the two ends of the run.
    ///
    /// Values, because row numbers do not survive the file being read again,
    /// while "from this sample to that one" does. The sort column is the one
    /// that made the run a run: with no sort the order is the row numbering,
    /// and then the numbers themselves are the honest description.
    pub fn crop_bounds(&self, from: usize, to: usize) -> (String, String, String) {
        match self.sort {
            Some(spec) => {
                let at = |i: usize| {
                    self.data
                        .cell_display(spec.col, self.orig_row(i))
                        .ok()
                        .flatten()
                        .unwrap_or_default()
                };
                (self.data.column_names[spec.col].clone(), at(from), at(to))
            }
            None => (
                String::new(),
                (self.orig_row(from) + 1).to_string(),
                (self.orig_row(to) + 1).to_string(),
            ),
        }
    }

    /// Where the two values that once bounded a crop sit in this tab's order
    /// now, as view rows.
    ///
    /// Found by looking for the values rather than by comparing against them:
    /// the run is contiguous because the tab is ordered by that column, so the
    /// first and last row carrying each value are its edges, and no notion of
    /// which way round the column sorts is needed here. `None` if either value
    /// has gone, which is the honest answer — a crop cannot be placed by
    /// guesswork.
    pub fn locate_crop(&self, column: &str, from: &str, to: &str) -> Option<(usize, usize)> {
        let total = self.row_count();
        if column.is_empty() {
            // Row-number order: the bounds are those numbers.
            let number = |text: &str| text.parse::<usize>().ok().map(|n| n.saturating_sub(1));
            let (first, last) = (number(from)?, number(to)?);
            let place = |wanted: usize| (0..total).find(|&i| self.orig_row(i) == wanted);
            return Some((place(first)?, place(last)?));
        }
        let col = self.data.column_names.iter().position(|n| n == column)?;
        let (mut start, mut end) = (None, None);
        // Read in blocks, so locating a crop in a large tab costs one pass and
        // not one chunk load per row.
        const BLOCK: usize = 8192;
        for base in (0..total).step_by(BLOCK) {
            let rows = self.view_slice(base, BLOCK);
            let cells = self.data.cells(col, &rows).ok()?;
            for (i, cell) in cells.iter().enumerate() {
                let text = cell.as_deref().unwrap_or_default();
                if start.is_none() && text == from {
                    start = Some(base + i);
                }
                if text == to {
                    end = Some(base + i);
                }
            }
        }
        let (start, end) = (start?, end?);
        match start <= end {
            true => Some((start, end)),
            // The column now sorts the other way round: the run is still the
            // same run, just walked from the other end.
            false => Some((end, start)),
        }
    }

    /// A window of the current view's rows, as original row indices — so a very
    /// large view can be walked a piece at a time without ever building the
    /// whole list.
    pub fn view_slice(&self, from: usize, len: usize) -> Vec<usize> {
        let end = (from + len).min(self.row_count());
        (from.min(end)..end).map(|i| self.orig_row(i)).collect()
    }

    /// Original row indices of the current view, capped at `max` (used to build
    /// the transposed table without unbounded width on large files).
    pub fn view_rows(&self, max: usize) -> Vec<usize> {
        (0..self.row_count().min(max)).map(|i| self.orig_row(i)).collect()
    }

    /// Turn the summary line on, or move it to the next thing it can show.
    /// A run of rapid repeats means the key is held, which puts the line away
    /// rather than spinning through the cycle.
    fn cycle_summary(&mut self, now: Instant) {
        let held = match &self.repeat {
            Some(r) if r.id == SUMMARY_REPEAT_ID => {
                now.saturating_duration_since(r.last) <= REPEAT_WINDOW
            }
            _ => false,
        };
        let count = match &self.repeat {
            Some(r) if held => r.count + 1,
            _ => 0,
        };
        self.repeat = Some(Repeat {
            id: SUMMARY_REPEAT_ID,
            last: now,
            count,
        });
        // Holding the key down runs through the cycle and then puts the line
        // away. Every press still moves it on, so tapping quickly is not
        // mistaken for a hold and left doing nothing.
        if count >= SUMMARY_HOLD {
            // Keep the run going: while the key stays down every further
            // repeat lands here and the line stays away, rather than being
            // switched back on by the next one.
            self.summary = None;
            self.summary_cols.clear();
            self.status_msg = Some("summary off".into());
            return;
        }
        // Turning the line on, and putting it away, are about the line rather
        // than any one column, so they take the whole of it either way.
        let Some(default) = self.summary else {
            self.summary = Some(Summary::Auto);
            self.summary_cols.clear();
            self.refresh_stats();
            self.status_msg = Some(format!("summary: {}", Summary::Auto.label()));
            return;
        };
        // Cycling is per column: the selected one moves on, and a pending
        // `(`/`)` brings the rest of the block along to the same thing.
        let next = self.summary_at(self.selected_col()).unwrap_or(default).next();
        for col in self.scoped_cols() {
            self.summary_cols.insert(col, next);
        }
        self.refresh_stats();
        self.note_scope(&format!("summary: {}", next.label()));
    }

    /// What `col` is set to show, before `Auto` is resolved.
    pub fn summary_at(&self, col: usize) -> Option<Summary> {
        let default = self.summary?;
        Some(self.summary_cols.get(&col).copied().unwrap_or(default))
    }

    /// Work out the totals for any column on display that hasn't got them yet.
    /// Cheap once they are in hand, which is what makes cycling `=` free.
    fn refresh_stats(&mut self) {
        if self.summary.is_none() {
            return;
        }
        let rows = self.filter.clone();
        let rows = rows.as_deref().map(Vec::as_slice);
        let missing: Vec<usize> = self
            .cols
            .iter()
            .copied()
            .filter(|c| !self.stats.contains_key(c) && self.data.is_numeric(*c))
            .collect();
        for col in missing {
            match self.data.column_stats(col, rows, interrupt::requested) {
                Ok(Some(stats)) => {
                    self.stats.insert(col, stats);
                }
                // Interrupted, or unreadable: leave it out and say nothing more
                // than the line already shows.
                Ok(None) => {
                    interrupt::take();
                    break;
                }
                Err(_) => break,
            }
        }
    }

    /// Forget the totals: the rows they were worked out over have changed.
    fn invalidate_stats(&mut self) {
        self.stats.clear();
        self.refresh_stats();
    }

    /// What the summary line shows for `col`, if anything.
    pub fn summary_of(&self, col: usize) -> Option<String> {
        let summary = self.summary_at(col)?;
        let stats = self.stats.get(&col)?;
        if stats.count == 0 {
            return None;
        }
        // `Auto` averages a log-scaled column, where a total says little.
        let mode = match summary {
            Summary::Auto => {
                let logged = self.num_styles.get(&col).is_some_and(|s| s.log);
                if logged {
                    Summary::Mean
                } else {
                    Summary::Total
                }
            }
            other => other,
        };
        let decimals = self.num_styles.get(&col).and_then(|s| s.decimals);
        let show = |v: f64| format_stat(v, decimals);
        match mode {
            Summary::Total => Some(show(stats.sum)),
            Summary::Mean => stats.mean().map(show),
            Summary::Stddev => stats.stddev().map(show),
            Summary::MeanStddev => {
                let (mean, sd) = (stats.mean()?, stats.stddev()?);
                Some(format!("{}±{}", show(mean), show(sd)))
            }
            Summary::Auto => unreachable!("resolved above"),
        }
    }

    /// The width set for `col`, if the user has chosen one.
    pub fn col_width(&self, col: usize) -> Option<u16> {
        self.col_widths.get(&col).copied()
    }

    /// Begin adjusting widths: this column, or this one and all those right of
    /// it. The current widths are remembered so `Esc` can put them back.
    fn start_resize(&mut self) {
        if self.cols.is_empty() {
            return;
        }
        let (from, to) = self.scoped_span();
        let count = to - from + 1;
        // A resize is a whole interaction rather than a repeatable keypress, so
        // it spends the scope on the way in.
        self.scope = None;
        let saved = self.cols[from..from + count]
            .iter()
            .map(|&col| (col, self.col_width(col)))
            .collect();
        self.record("resize");
        let selected = self.cols[self.selected_pos.min(self.cols.len() - 1)];
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
            KeyCode::Char('0') => {
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
                // Put back exactly what was there before — which is what the
                // step recorded on entry holds, so that step goes too.
                self.discard_record();
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

    /// Begin adding a column, worked out from the ones already there.
    fn start_new_column(&mut self) {
        if self.cols.is_empty() {
            return;
        }
        self.new_column = Some(NewColumn {
            kind: None,
            source: self.selected_col(),
            recipe: None,
        });
    }

    /// Choose between a regex extraction and a formula.
    fn handle_new_column_kind(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        let kind = match key.code {
            KeyCode::Char('e') => NewKind::Extract,
            KeyCode::Char('f') => NewKind::Formula,
            KeyCode::Esc => {
                self.new_column = None;
                self.status_msg = Some("no new column".into());
                return;
            }
            _ => return,
        };
        if let Some(new) = &mut self.new_column {
            new.kind = Some(kind);
        }
        self.enter_input(InputKind::Recipe);
    }

    /// Take the pattern or formula, then ask what to call the column.
    ///
    /// Checked here rather than at the end: learning that a formula will not do
    /// only after naming the column would mean typing the whole thing again.
    fn take_recipe(&mut self, text: String) {
        if text.trim().is_empty() {
            self.new_column = None;
            self.status_msg = Some("no new column".into());
            return;
        }
        let Some(new) = self.new_column.as_ref() else { return };
        let trial = match new.kind {
            Some(NewKind::Extract) => Recipe::Extract {
                source: self.data.column_names[new.source].clone(),
                pattern: text.clone(),
            },
            _ => Recipe::Formula {
                expression: text.clone(),
            },
        };
        if let Err(e) = self.data.validate_recipe(&trial) {
            // Back to the prompt with the text still there, and the trouble
            // shown over the table until it is edited.
            let detail = e.downcast_ref::<FormulaError>();
            self.notice = Some(Notice {
                title: "formula",
                subject: Some(text.clone()),
                at: detail.and_then(|d| d.at),
                message: detail
                    .map(|d| d.message.clone())
                    .unwrap_or_else(|| format!("{e}")),
                hint: detail.and_then(|d| d.hint.clone()),
                // The prompt is still up: this goes when the text changes.
                dismissable: false,
                footer: " keep typing to fix it · Esc gives up ",
            });
            self.mode = Mode::Input(InputKind::Recipe);
            self.input = text;
            return;
        }
        let suggestion = match self.new_column.as_mut() {
            Some(new) => {
                new.recipe = Some(text);
                match new.kind {
                    Some(NewKind::Extract) => {
                        format!("{}_part", self.data.column_names[new.source])
                    }
                    _ => "computed".to_string(),
                }
            }
            None => return,
        };
        self.mode = Mode::Input(InputKind::ColumnName);
        self.input = suggestion;
    }

    /// Add the column, or rename the selected one — whichever this name is for.
    fn take_column_name(&mut self, name: String) {
        let Some(new) = self.new_column.take() else {
            return self.rename_column(name);
        };
        let (Some(kind), Some(text)) = (new.kind, new.recipe) else {
            return;
        };
        let recipe = match kind {
            NewKind::Extract => Recipe::Extract {
                source: self.data.column_names[new.source].clone(),
                pattern: text,
            },
            NewKind::Formula => Recipe::Formula { expression: text },
        };
        self.record("new column");
        match self.data.add_computed(&name, recipe) {
            Ok(()) => {
                // Shown at the end, where it was added.
                let col = self.data.ncols - 1;
                self.cols.push(col);
                self.selected_pos = self.last_col();
                self.refresh_stats();
                self.status_msg = Some(format!("added {name}"));
            }
            Err(e) => {
                self.discard_record();
                self.status_msg = Some(format!("{e}"));
            }
        }
    }

    /// Rename the selected column.
    fn rename_column(&mut self, name: String) {
        if name.trim().is_empty() {
            return;
        }
        let col = self.selected_col();
        let was = self.data.column_names[col].clone();
        self.record("rename");
        match self.data.rename(col, &name) {
            Ok(()) => self.status_msg = Some(format!("{was} → {name}")),
            Err(e) => {
                self.discard_record();
                self.status_msg = Some(format!("{e}"));
            }
        }
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
        self.record("sort");
        self.sort = Some(SortSpec {
            col: wizard.col,
            dir: SortDir::Asc,
            key: Some(key),
        });
        if !self.rebuild_view() {
            interrupt::take();
            self.discard_record();
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
        // A pending scope lasts for a run of column commands — `( % > =` all
        // land on the same block — and is dropped by anything else, so it can
        // never quietly apply to a command typed much later.
        if !is_column_command(key.code) {
            self.scope = None;
        }
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('c') if ctrl => self.should_quit = true,
            // Esc peels away search, then filter, then leaves a transposed
            // view (or quits at the top level).
            KeyCode::Esc => {
                if self.crop_mark.take().is_some() {
                    self.status_msg = Some("crop dropped".into());
                } else if self.join_active {
                    self.cancel_join = true;
                } else if self.search.is_some() {
                    self.record("clear search");
                    self.search = None;
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
            // `w` writes one file's arrangement; `W` writes the whole folder's;
            // `X` writes the table itself.
            KeyCode::Char('X') => self.enter_input(InputKind::Export),
            KeyCode::Char('W') => self.save_session = true,
            KeyCode::Char('w') if !ctrl => {
                // Asked before the name, not after: there is no point
                // collecting one for something that cannot be saved.
                if self.is_file_backed() {
                    self.enter_input(InputKind::Pattern);
                } else {
                    self.notice = Some(
                        Notice::say(
                            "pattern",
                            "a pattern belongs to a file, and this view is not one",
                        )
                        .hint(
                            "a join and a transposed view are worked out from other \
                             tabs, so there is no file to tie an arrangement to"
                                .to_string(),
                        ),
                    );
                }
            }
            KeyCode::Char('?') => self.show_help = true,
            // `J` starts the join wizard; `Enter` then picks the key column
            // under the cursor, once on each side.
            KeyCode::Char('J') => self.join_request = true,
            // `c` marks one edge of a crop and then the other. What lies
            // between becomes a tab of its own, so nothing here is disturbed
            // and there is nothing to undo — the crop is closed by closing it.
            KeyCode::Char('c') => self.mark_crop(),
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
            // `(` and `)` aim the next column command at a block of columns.
            KeyCode::Char('(') => self.scope = Some(Scope::Rightward),
            KeyCode::Char(')') => self.scope = Some(Scope::Leftward),
            KeyCode::Char('%') => self.toggle_numeric(),
            KeyCode::Char('>') => self.adjust_decimals(1),
            KeyCode::Char('<') => self.adjust_decimals(-1),
            KeyCode::Char('s') => self.cycle_sort(),
            KeyCode::Char('S') => self.start_key_sort(),
            // `r` adjusts this column's width, `R` this one and every column
            // to its right.
            // `=` turns the summary line on and cycles it; holding it down puts
            // the line away.
            KeyCode::Char('=') => self.cycle_summary(now),
            // `a` adds a column worked out from the others; `R` renames one.
            KeyCode::Char('a') => self.start_new_column(),
            KeyCode::Char('R') => self.enter_input(InputKind::ColumnName),
            KeyCode::Char('z') => self.undo(),
            KeyCode::Char('Z') => self.redo(),
            KeyCode::Char('r') => self.start_resize(),
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
        // Any edit clears the last complaint: it described text that has
        // changed since, and a stale caret points at nothing.
        if !matches!(key.code, KeyCode::Enter) {
            self.notice = None;
        }
        // The open prompt browses the filesystem; the others are plain text.
        // Both prompts that name a file browse for it.
        if matches!(kind, InputKind::Open | InputKind::Export) && self.handle_open_key(key, kind) {
            return;
        }
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.input.clear();
                self.completions = None;
                self.notice = None;
                if self.new_column.take().is_some() {
                    self.status_msg = Some("no new column".into());
                }
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
    fn handle_open_key(&mut self, key: KeyEvent, kind: InputKind) -> bool {
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
            KeyCode::Enter if self.completions.is_some() => self.accept_completion(kind),
            // `Esc` puts the picker away first, leaving what was typed.
            KeyCode::Esc if self.completions.is_some() => self.completions = None,
            _ => return false,
        }
        true
    }

    /// The folder a bare name is taken against: where the file on screen lives.
    /// Where the prompt now open takes a relative path from. `o` works from
    /// the file on screen, since that is what you are looking at and near.
    /// `X` works from the folder lambris was started in: a bare name typed
    /// there should land where the command was typed, not beside whichever
    /// input a join happened to start from — which is somewhere the tab itself
    /// never mentions.
    fn prompt_dir(&self) -> std::path::PathBuf {
        match self.mode {
            Mode::Input(InputKind::Export) => self.folder.clone(),
            _ => self.base_dir(),
        }
    }

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
        let listing = Completions::for_input(&self.input, &self.prompt_dir());
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
    fn accept_completion(&mut self, kind: InputKind) {
        let Some(listing) = &self.completions else { return };
        let Some(entry_is_dir) = listing.selected_entry().map(|e| e.is_dir) else {
            return;
        };
        let Some(text) = listing.selected_input() else { return };
        self.input = text;
        if entry_is_dir {
            return self.open_completions(false); // list what is inside it
        }
        self.completions = None;
        // Picking an existing file when opening one means open it. When writing
        // one out it means write over it, which is not something a single key
        // should set going: it fills the name in and waits.
        if let InputKind::Open = kind {
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
            // Offered pre-filled with the file's name, which is the binding
            // most patterns want — and editable into a glob for the rest.
            InputKind::Pattern => Some(self.pattern_bind()),
            // Renaming starts from the name it has now.
            InputKind::ColumnName => Some(self.data.column_names[self.selected_col()].clone()),
            InputKind::Goto | InputKind::Open | InputKind::Recipe | InputKind::Export => None,
        }
        .unwrap_or_default();
    }

    fn commit_input(&mut self, kind: InputKind) {
        let query = std::mem::take(&mut self.input);
        self.mode = Mode::Normal;
        self.completions = None;
        // These take the text literally rather than as a regex.
        match kind {
            InputKind::Goto => return self.goto_line(&query),
            InputKind::Pattern => {
                self.save_pattern = Some(query.trim().to_string());
                return;
            }
            InputKind::Export => {
                let name = query.trim();
                if !name.is_empty() {
                    self.export_request = Some(name.to_string());
                }
                return;
            }
            InputKind::Recipe => return self.take_recipe(query),
            InputKind::ColumnName => return self.take_column_name(query),
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
                InputKind::Goto
                | InputKind::Open
                | InputKind::Pattern
                | InputKind::Recipe
                | InputKind::ColumnName
                | InputKind::Export => unreachable!(),
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
            InputKind::Goto
                | InputKind::Open
                | InputKind::Pattern
                | InputKind::Recipe
                | InputKind::ColumnName
                | InputKind::Export => unreachable!(),
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
        self.record("search");
        self.search = Some(Search { query, re, scope });
        // Land on the first match from the current position (inclusive-ish).
        self.jump_match(true);
    }

    fn apply_filter(&mut self, query: String, re: Regex) {
        self.record("filter");
        match self.data.filter_rows(self.visible_cols(), &re, interrupt::requested) {
            Ok(Some(rows)) => {
                let n = rows.len();
                self.filter = Some(Arc::new(rows));
                self.filter_query = Some(query);
                self.selected_row = 0;
                self.row_offset = 0;
                self.invalidate_stats();
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
                self.discard_record();
                self.status_msg = Some("filter cancelled".into());
            }
            Err(e) => {
                self.discard_record();
                self.status_msg = Some(format!("filter failed: {e}"));
            }
        }
    }

    fn clear_filter(&mut self) {
        self.record("clear filter");
        self.filter_query = None;
        self.filter = None;
        self.invalidate_stats();
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
        self.record("sort");
        self.sort = next.map(|dir| SortSpec { col, dir, key });
        if !self.rebuild_view() {
            // Sort aborted; restore the previous ordering (the view is untouched).
            interrupt::take();
            self.discard_record();
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

    /// Style the selected column, and every column a pending `(`/`)` covers,
    /// the same way. Columns that hold no numbers are passed over.
    fn toggle_numeric(&mut self) {
        let selected = self.selected_col();
        if !self.data.is_numeric(selected) {
            self.status_msg = Some("column is not numeric".into());
            return;
        }
        let mut style = self.num_styles.get(&selected).copied().unwrap_or_default();
        style.log = !style.log;
        style.align = style.log || style.decimals.is_some();
        self.record("numeric style");
        self.apply_num_style(style);
        let what = match (style.align, style.log) {
            (false, _) => "plain",
            (_, true) => "numeric + log colour",
            _ => "numeric",
        };
        self.note_scope(what);
    }

    /// Give every covered numeric column this style, or clear it when the style
    /// is doing nothing.
    fn apply_num_style(&mut self, style: NumStyle) {
        for col in self.scoped_cols() {
            if !self.data.is_numeric(col) {
                continue;
            }
            if style.align {
                self.num_styles.insert(col, style);
            } else {
                self.num_styles.remove(&col);
            }
        }
    }

    /// Set the decimals shown, on the selected column and any block with it.
    fn adjust_decimals(&mut self, delta: isize) {
        let selected = self.selected_col();
        if !self.data.is_numeric(selected) {
            self.status_msg = Some("column is not numeric".into());
            return;
        }
        let mut style = self.num_styles.get(&selected).copied().unwrap_or_default();
        let current = style.decimals.unwrap_or(2) as isize;
        let places = (current + delta).clamp(0, 10) as u8;
        style.decimals = Some(places);
        style.align = true;
        self.record("decimals");
        self.apply_num_style(style);
        self.note_scope(&format!("{places} decimals"));
    }

    /// Pin columns `0..=selected` to the left, or unfreeze if already there.
    fn toggle_freeze(&mut self) {
        if self.data.ncols == 0 {
            return;
        }
        self.record("freeze");
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
                    Some(f) => f.as_ref().clone(),
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
                    Ok(Some(rows)) => View::Rows(Arc::new(rows)),
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

/// A statistic as text: whole numbers plainly, otherwise enough decimals to be
/// worth reading, or the column's own fixed decimals when it has them.
fn format_stat(value: f64, decimals: Option<u8>) -> String {
    if let Some(n) = decimals {
        return format!("{value:.*}", n as usize);
    }
    if value == value.trunc() && value.abs() < 1e15 {
        return format!("{value:.0}");
    }
    // Big numbers do not need fractions; small ones are mostly fraction.
    let places = match value.abs() {
        v if v >= 1e6 => 0,
        v if v >= 1.0 => 2,
        _ => 4,
    };
    format!("{value:.*}", places)
}

/// Whether a `(`/`)` scope applies to this key, and so survives it.
fn is_column_command(code: KeyCode) -> bool {
    matches!(
        code,
        KeyCode::Char('(' | ')' | 'r' | '%' | '<' | '>' | '=' | 'x')
    )
}

/// Compile a user query as a case-insensitive regex (csvlens-style search).
fn build_regex(query: &str) -> Result<Regex, regex::Error> {
    RegexBuilder::new(query).case_insensitive(true).build()
}

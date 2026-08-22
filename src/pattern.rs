//! Saved per-file view patterns: the arrangement of a table, remembered.
//!
//! A pattern records what a view *looks like* by column **name**, never by
//! position — a column moved to third place is stored as a name in a list, so
//! the arrangement survives a file gaining, losing or reordering columns. It is
//! derived from the live view when the user asks to save it, so there is no
//! parallel bookkeeping to drift out of step with the view it describes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Environment variable pointing at the pattern file, chiefly so tests never
/// touch the real one.
const CONFIG_ENV: &str = "LAMBRIS_CONFIG";

/// One saved arrangement. Everything optional, so a hand-written pattern can
/// set one thing and leave the rest alone.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Pattern {
    /// The file this is tied to: a name, or a glob over one (`*stats1.tsv`).
    /// Empty when the arrangement belongs to a session rather than a file.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub bind: String,
    /// Which worksheet, when the file is a workbook.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sheet: Option<String>,
    /// Visible columns by name, in display order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub columns: Vec<String>,
    /// Columns that were hidden. A column in neither list is one the file has
    /// gained since, and stays visible rather than disappearing quietly.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hidden: Vec<String>,
    /// Widths set by hand, by column name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub widths: BTreeMap<String, u16>,
    /// Numeric display styles, by column name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub numeric: BTreeMap<String, SavedNumStyle>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<SavedSort>,
    /// Frozen through this column, inclusive — a name rather than a count, so
    /// it still means the same thing if a column before it has gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frozen_through: Option<String>,
    /// The row filter, as the regex that produced it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// How the top of the file is read (`T`/`H`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<SavedHeader>,
    /// Whether the row-number gutter is shown.
    #[serde(default = "yes")]
    pub row_numbers: bool,
    /// Whether the summary line is on, and what a column shows unless it says
    /// otherwise: `auto`, `total`, `mean`, `sd` or `mean-sd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Columns showing something other than that, by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub summaries: BTreeMap<String, String>,
    /// Columns worked out from the others, in the order they were added — a
    /// later one may refer to an earlier one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub computed: Vec<SavedColumn>,
    /// Columns renamed, from the name the file gives them to the new one.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub renamed: BTreeMap<String, String>,
}

fn yes() -> bool {
    true
}

/// A computed column: its name and how it is worked out. Exactly one of
/// `extract` (with `from`) or `formula` is set.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SavedColumn {
    pub name: String,
    /// The column an extraction reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// The regex whose first capture becomes the value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extract: Option<String>,
    /// An expression over the columns: `{a} + "x"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub formula: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct SavedNumStyle {
    pub align: bool,
    pub log: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decimals: Option<u8>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SavedSort {
    pub column: String,
    pub descending: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<SavedSortKey>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SavedSortKey {
    /// Character offsets, as `sort -k` would write them: 1-based, inclusive.
    pub from: usize,
    pub to: usize,
    /// `abc`, `num` or `nat` — a short string so a hand-edited pattern reads
    /// plainly, and an unknown value can fall back rather than fail to parse.
    pub method: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct SavedHeader {
    /// Rows ignored above the header.
    pub skip: usize,
    /// Whether the row after them holds the column names.
    pub named: bool,
}

/// Every saved pattern, and the file they live in.
///
/// The path is carried rather than looked up on each write, so a caller — a
/// test, most of all — can point a store somewhere harmless without reaching
/// for a process-wide environment variable.
pub struct Store {
    patterns: Vec<Pattern>,
    path: PathBuf,
}

impl Default for Store {
    /// Empty, aimed at the usual file. Nothing is written until `save`.
    fn default() -> Self {
        Self {
            patterns: Vec::new(),
            path: Self::default_path(),
        }
    }
}

impl Store {
    /// Where patterns are kept: `$LAMBRIS_CONFIG` if set, else
    /// `$XDG_CONFIG_HOME/lambris/patterns.json`, else
    /// `~/.config/lambris/patterns.json` — the same path on every unix-ish
    /// system, which is what command-line tools tend to settle on.
    pub fn default_path() -> PathBuf {
        if let Some(explicit) = std::env::var_os(CONFIG_ENV) {
            return PathBuf::from(explicit);
        }
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("lambris").join("patterns.json")
    }

    /// Read the saved patterns from the usual place.
    pub fn load() -> Self {
        Self::load_at(Self::default_path())
    }

    /// Read them from a given file. A missing or unreadable one is simply no
    /// patterns: a viewer should still open the file you asked for.
    pub fn load_at(path: PathBuf) -> Self {
        let patterns = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Vec<Pattern>>(&text).ok())
            .unwrap_or_default();
        Self { patterns, path }
    }

    /// Write them back, pretty-printed so the file can be edited by hand.
    pub fn save(&self) -> Result<()> {
        let path = &self.path;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating {}", dir.display()))?;
        }
        let text = serde_json::to_string_pretty(&self.patterns)
            .context("encoding patterns")?;
        std::fs::write(path, text + "\n")
            .with_context(|| format!("writing {}", path.display()))
    }

    /// The pattern that applies to `path` (and `sheet`, for a workbook).
    ///
    /// An exact name wins over a glob, and among globs the longest binding
    /// wins — the more specific `*stats1.tsv` beating a blanket `*.tsv`.
    pub fn matching(&self, path: &Path, sheet: Option<&str>) -> Option<&Pattern> {
        let mut best: Option<&Pattern> = None;
        for pattern in &self.patterns {
            if pattern.sheet.as_deref() != sheet || !binds(&pattern.bind, path) {
                continue;
            }
            let better = match best {
                None => true,
                Some(current) => {
                    let exact = |p: &Pattern| !is_glob(&p.bind);
                    match (exact(pattern), exact(current)) {
                        (true, false) => true,
                        (false, true) => false,
                        _ => pattern.bind.len() > current.bind.len(),
                    }
                }
            };
            if better {
                best = Some(pattern);
            }
        }
        best
    }

    /// Add a pattern, replacing any with the same binding and sheet.
    pub fn put(&mut self, pattern: Pattern) {
        self.forget(&pattern.bind, pattern.sheet.as_deref());
        self.patterns.push(pattern);
    }

    /// Drop the pattern with this binding and sheet, if there is one.
    pub fn forget(&mut self, bind: &str, sheet: Option<&str>) -> bool {
        let before = self.patterns.len();
        self.patterns
            .retain(|p| !(p.bind == bind && p.sheet.as_deref() == sheet));
        self.patterns.len() != before
    }
}

/// Every tab that was open in a folder, and how each was arranged.
///
/// A pattern says how *a kind of file* should look wherever it turns up; a
/// session says what was open *here*. The two answer different questions, which
/// is why one is tied to a name or a glob and the other to a folder.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Session {
    /// The folder this belongs to, as an absolute path.
    pub folder: String,
    pub tabs: Vec<SessionTab>,
    /// Which tab was in front.
    #[serde(default)]
    pub current: usize,
}

/// One tab of a session: which table, and how it looked.
///
/// A table is either read from a file or joined from two tabs saved before this
/// one. A join keeps no data of its own, so it is written down as the recipe it
/// is and made again when the session reopens.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct SessionTab {
    /// Relative to the folder when the file is inside it, absolute otherwise —
    /// so a project that is copied elsewhere still opens. Empty for a join or a crop.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sheet: Option<String>,
    /// Set when this tab was joined rather than read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub join: Option<SavedJoin>,
    /// Set when this tab was cropped out of another.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crop: Option<SavedCrop>,
    /// Whether the tab was showing a transposed view.
    #[serde(default, skip_serializing_if = "is_false")]
    pub transposed: bool,
    /// The arrangement, recorded exactly as a pattern records one.
    #[serde(default)]
    pub view: Pattern,
}

/// A join, as the two tabs and key columns it was made from.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct SavedJoin {
    /// Which tabs of this session, always ones saved before this one — so a
    /// join of a join works, and the whole lot rebuilds in one pass.
    pub left: usize,
    pub right: usize,
    /// The key columns, by name.
    pub left_key: String,
    pub right_key: String,
}

/// A crop, as a session writes it down: which tab it was taken from, and the
/// two values that bounded the run in that tab's sort column. Values rather
/// than row numbers, so the same run can be found again once the file has been
/// read afresh. `column` is empty when the tab was in row-number order, and
/// then the bounds are those numbers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedCrop {
    pub source: usize,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub column: String,
    pub from: String,
    pub to: String,
    /// How many rows it held when it was taken. Checked on the way back, since
    /// a boundary value shared by several rows takes all of them in.
    pub rows: usize,
    /// The columns kept, by name and in the order they were shown. Unlike the
    /// rows, these need no locating — which is why only one half of a crop is
    /// described by where it began and ended.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub columns: Vec<String>,
}

fn is_false(value: &bool) -> bool {
    !value
}

/// The saved sessions, one per folder.
pub struct Sessions {
    sessions: Vec<Session>,
    path: PathBuf,
}

impl Default for Sessions {
    fn default() -> Self {
        Self {
            sessions: Vec::new(),
            path: Self::default_path(),
        }
    }
}

impl Sessions {
    /// Beside the patterns, and found the same way.
    pub fn default_path() -> PathBuf {
        let mut path = Store::default_path();
        path.set_file_name("sessions.json");
        path
    }

    pub fn load() -> Self {
        Self::load_at(Self::default_path())
    }

    pub fn load_at(path: PathBuf) -> Self {
        let sessions = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Vec<Session>>(&text).ok())
            .unwrap_or_default();
        Self { sessions, path }
    }

    pub fn save(&self) -> Result<()> {
        let path = &self.path;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating {}", dir.display()))?;
        }
        let text =
            serde_json::to_string_pretty(&self.sessions).context("encoding sessions")?;
        std::fs::write(path, text + "\n")
            .with_context(|| format!("writing {}", path.display()))
    }

    /// What was open in this folder, if anything.
    pub fn for_folder(&self, folder: &Path) -> Option<&Session> {
        let folder = folder.to_string_lossy();
        self.sessions.iter().find(|s| s.folder == folder)
    }

    /// Remember this folder's session, replacing whatever was there.
    pub fn put(&mut self, session: Session) {
        self.sessions.retain(|s| s.folder != session.folder);
        self.sessions.push(session);
    }

    /// Forget a folder's session.
    pub fn forget(&mut self, folder: &Path) -> bool {
        let folder = folder.to_string_lossy();
        let before = self.sessions.len();
        self.sessions.retain(|s| s.folder != folder);
        self.sessions.len() != before
    }
}

/// Where a file sits relative to a folder — a plain name when it is inside it,
/// the whole path when it is not.
pub fn relative_to(folder: &Path, file: &Path) -> String {
    file.strip_prefix(folder)
        .map(|rest| rest.to_string_lossy().into_owned())
        .unwrap_or_else(|_| file.to_string_lossy().into_owned())
}

/// The other way round: a name is taken as being inside the folder.
pub fn resolve_in(folder: &Path, file: &str) -> PathBuf {
    let path = PathBuf::from(file);
    match path.is_absolute() {
        true => path,
        false => folder.join(path),
    }
}

fn is_glob(bind: &str) -> bool {
    bind.contains('*') || bind.contains('?')
}

/// Does `bind` name this file? A binding with a `/` in it is matched against
/// the whole path, anything else against the file name alone — so a pattern
/// follows a file that gets regenerated in another directory, unless it was
/// deliberately tied to one place.
fn binds(bind: &str, path: &Path) -> bool {
    let target = if bind.contains('/') {
        path.to_string_lossy().into_owned()
    } else {
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    if !is_glob(bind) {
        return bind == target;
    }
    glob_regex(bind).is_match_at(&target, 0)
}

/// Turn a glob into an anchored regex: `*` and `?` are wildcards and everything
/// else is literal. Reuses the regex engine already in the binary rather than
/// hand-rolling a matcher.
fn glob_regex(bind: &str) -> regex::Regex {
    let mut expression = String::with_capacity(bind.len() * 2 + 4);
    expression.push('^');
    for c in bind.chars() {
        match c {
            '*' => expression.push_str(".*"),
            '?' => expression.push('.'),
            other => expression.push_str(&regex::escape(&other.to_string())),
        }
    }
    expression.push('$');
    regex::Regex::new(&expression).unwrap_or_else(|_| regex::Regex::new("$^").unwrap())
}

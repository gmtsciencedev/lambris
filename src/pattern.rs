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
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Pattern {
    /// The file this is tied to: a name, or a glob over one (`*stats1.tsv`).
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
}

fn yes() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default)]
pub struct SavedNumStyle {
    pub align: bool,
    pub log: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decimals: Option<u8>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SavedSort {
    pub column: String,
    pub descending: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<SavedSortKey>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SavedSortKey {
    /// Character offsets, as `sort -k` would write them: 1-based, inclusive.
    pub from: usize,
    pub to: usize,
    /// `abc`, `num` or `nat` — a short string so a hand-edited pattern reads
    /// plainly, and an unknown value can fall back rather than fail to parse.
    pub method: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
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

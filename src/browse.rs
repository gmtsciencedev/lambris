//! Path completion for the open prompt: what `Tab` offers while typing a path.
//!
//! Everything here is a snapshot — the directory is read when the list is built
//! or refreshed, never while rendering.

use std::path::{Path, PathBuf};

/// How many entries the picker shows at once before it scrolls.
pub const VISIBLE: usize = 10;

/// One candidate in the picker.
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
}

impl Entry {
    /// The name as shown, directories marked so they read as somewhere to go.
    pub fn label(&self) -> String {
        if self.is_dir {
            format!("{}/", self.name)
        } else {
            self.name.clone()
        }
    }
}

/// The candidates for a partially typed path, and which one is highlighted.
pub struct Completions {
    /// The directory these entries came from.
    pub dir: PathBuf,
    /// What was typed after it; entries all start with this.
    pub prefix: String,
    pub entries: Vec<Entry>,
    pub selected: usize,
    /// Why the list is empty, when it is.
    pub note: Option<String>,
}

impl Completions {
    /// Build the candidate list for `input`. An empty input lists `fallback` —
    /// the folder of the file being viewed, which is where the next file to
    /// open usually lives. Never fails: an unreadable directory comes back as
    /// an empty list carrying the reason.
    pub fn for_input(input: &str, base: &Path) -> Self {
        let (dir, prefix) = split_input(input, base);
        let mut entries = Vec::new();
        let mut note = None;
        match std::fs::read_dir(&dir) {
            Ok(reader) => {
                let wanted = prefix.to_lowercase();
                for entry in reader.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    // Hidden files stay hidden until asked for by name.
                    if name.starts_with('.') && !prefix.starts_with('.') {
                        continue;
                    }
                    if !name.to_lowercase().starts_with(&wanted) {
                        continue;
                    }
                    let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    entries.push(Entry { name, is_dir });
                }
                // Directories first — they are the way onwards — then by name.
                entries.sort_by(|a, b| {
                    b.is_dir
                        .cmp(&a.is_dir)
                        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                });
                if entries.is_empty() {
                    note = Some(if prefix.is_empty() {
                        "empty directory".to_string()
                    } else {
                        format!("nothing starting with {prefix}")
                    });
                }
            }
            Err(e) => note = Some(format!("cannot read: {e}")),
        }
        Self {
            dir,
            prefix,
            entries,
            selected: 0,
            note,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Move the highlight, wrapping at both ends.
    pub fn step(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let n = self.entries.len() as isize;
        self.selected = (((self.selected as isize + delta) % n + n) % n) as usize;
    }

    pub fn selected_entry(&self) -> Option<&Entry> {
        self.entries.get(self.selected)
    }

    /// The typed text that selecting the highlighted entry produces: a
    /// directory gains a trailing slash so the next `Tab` steps into it.
    pub fn selected_input(&self) -> Option<String> {
        let entry = self.selected_entry()?;
        let path = self.dir.join(&entry.name);
        let mut text = path.to_string_lossy().into_owned();
        if entry.is_dir {
            text.push('/');
        }
        Some(text)
    }

    /// The longest prefix every candidate shares, for shell-style completion.
    /// `None` when it adds nothing to what was typed.
    pub fn common_prefix(&self) -> Option<String> {
        let first = self.entries.first()?;
        let mut common = first.name.clone();
        for entry in &self.entries[1..] {
            let shared = entry
                .name
                .char_indices()
                .zip(common.chars())
                .take_while(|((_, a), b)| a.eq_ignore_ascii_case(b))
                .count();
            common.truncate(
                common
                    .char_indices()
                    .nth(shared)
                    .map(|(i, _)| i)
                    .unwrap_or(common.len()),
            );
        }
        (common.chars().count() > self.prefix.chars().count()).then(|| {
            let mut text = self.dir.join(&common).to_string_lossy().into_owned();
            // A lone match that is a directory can be stepped into right away.
            if self.entries.len() == 1 && self.entries[0].is_dir {
                text.push('/');
            }
            text
        })
    }

    /// The window of entries to draw, so a long listing scrolls with the
    /// highlight instead of running off the box.
    pub fn window(&self, height: usize) -> (usize, &[Entry]) {
        if self.entries.is_empty() || height == 0 {
            return (0, &[]);
        }
        let height = height.min(self.entries.len());
        let start = self
            .selected
            .saturating_sub(height - 1)
            .min(self.entries.len() - height);
        (start, &self.entries[start..start + height])
    }
}

/// Split a typed path into the directory to list and the prefix to match.
///
/// The split is on the last `/` in the *text*, not by path semantics, so a
/// leading `.` reads as "show me the hidden ones" rather than as the current
/// directory.
fn split_input(input: &str, base: &Path) -> (PathBuf, String) {
    match input.rsplit_once('/') {
        // No separator: a bare prefix against the folder an empty input lists.
        None => (base.to_path_buf(), input.to_string()),
        Some((dir, name)) => (resolve(&format!("{dir}/"), base), name.to_string()),
    }
}

/// Resolve a typed path: `~/` expands, and anything relative is taken against
/// `base` — the folder of the file being viewed, which is what the picker
/// lists — so typing and picking agree on where a name points.
pub fn resolve(input: &str, base: &Path) -> PathBuf {
    if let Some(rest) = input.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    let path = PathBuf::from(input);
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

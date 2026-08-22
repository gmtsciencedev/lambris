mod app;
mod browse;
mod data;
mod formula;
mod pattern;
mod interrupt;
mod ui;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{self, Event, KeyEventKind};

use app::App;
use data::{Dataset, HeaderSpec, JoinSide, JOIN_MAX_ROWS};
use pattern::{relative_to, resolve_in, SavedJoin, Session, SessionTab, Sessions, Store};

/// A terminal viewer for parquet, CSV/TSV (plain or gzipped) and Excel files,
/// in the manner of csvlens.
#[derive(Parser)]
#[command(name = "lambris", version, about)]
struct Args {
    /// Paths to the data files to view; each one opens in its own tab. With
    /// none, the session saved for this folder is reopened.
    files: Vec<PathBuf>,

    /// Treat the first row as data, not column names (columns become
    /// `column_N`). Toggle it per tab with `T`. Ignored for parquet.
    #[arg(long)]
    no_header: bool,

    /// Open files as they come, ignoring any arrangement saved with `w`.
    #[arg(long)]
    no_pattern: bool,

    /// Forget the session saved for this folder, and open nothing.
    #[arg(long)]
    forget_session: bool,
}

/// Largest number of records turned into columns when transposing, so a huge
/// file can't produce an unbounded number of columns.
const TRANSPOSE_MAX_RECORDS: usize = 4096;

fn main() -> Result<()> {
    let args = Args::parse();
    let header = if args.no_header {
        HeaderSpec::NONE
    } else {
        HeaderSpec::default()
    };
    // Saved arrangements are consulted before the files are read: how the top
    // of a file is read decides its schema, so it cannot be applied afterwards.
    let patterns = if args.no_pattern {
        Store::default()
    } else {
        Store::load()
    };
    let here = std::env::current_dir().context("finding the current folder")?;
    let mut sessions = Sessions::load();
    if args.forget_session {
        let forgotten = sessions.forget(&here);
        sessions.save()?;
        println!(
            "{}",
            match forgotten {
                true => format!("forgot the session for {}", here.display()),
                false => format!("no session saved for {}", here.display()),
            }
        );
        return Ok(());
    }
    // With no files named, pick up what was open here last.
    let mut tabs = match args.files.is_empty() {
        true => Tabs::reopen(&here, &sessions, header, patterns)?,
        false => Tabs::open(&args.files, header, patterns)?,
    };
    tabs.folder = here;
    tabs.sessions = sessions;

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut tabs);
    ratatui::restore();
    result
}

/// The open files. Each tab owns a stack of views — the base table plus any
/// transposed views pushed on top — so transposing or filtering one tab leaves
/// the others untouched, and only the top of the current tab's stack is drawn.
struct Tabs {
    tabs: Vec<Vec<App>>,
    current: usize,
    /// The join wizard, while one is running.
    join: Option<JoinWizard>,
    /// Saved arrangements, applied as files are opened and written back by `w`.
    patterns: Store,
    /// The folder a session belongs to.
    folder: PathBuf,
    /// Sessions, one per folder, written back by `W`.
    sessions: Sessions,
}

/// The join wizard: the user walks to a key column and confirms, once on each
/// side. Tabs and columns are picked with the ordinary movement keys, so the
/// wizard only has to remember the first pick.
struct JoinWizard {
    /// The first pick: which tab, and which of its columns.
    left: Option<(usize, usize)>,
}

impl Tabs {
    /// Open one tab per table, failing before the TUI starts if any path won't
    /// load. A workbook contributes one tab per sheet that holds data.
    fn open(paths: &[PathBuf], header: HeaderSpec, patterns: Store) -> Result<Self> {
        let mut tabs = Vec::with_capacity(paths.len());
        for path in paths {
            // A pattern tied to the file as a whole can decide how its top is
            // read, which has to be settled before the file is decoded.
            let header = patterns
                .matching(path, None)
                .and_then(App::header_from)
                .unwrap_or(header);
            let datasets = Dataset::load_all(path, header)
                .with_context(|| format!("loading {}", path.display()))?;
            for dataset in datasets {
                tabs.push(vec![App::new(dataset)]);
            }
        }
        if tabs.is_empty() {
            anyhow::bail!("nothing to show");
        }
        let mut opened = Self {
            tabs,
            current: 0,
            join: None,
            patterns,
            folder: PathBuf::from("."),
            sessions: Sessions::default(),
        };
        for tab in 0..opened.tabs.len() {
            opened.apply_saved_pattern(tab);
        }
        Ok(opened)
    }

    /// Reopen what was last open in `folder`.
    ///
    /// Each tab's own arrangement comes from the session rather than from a
    /// pattern: a session is a snapshot of how things actually were, so it wins
    /// over the general rule for that kind of file.
    fn reopen(
        folder: &Path,
        sessions: &Sessions,
        header: HeaderSpec,
        patterns: Store,
    ) -> Result<Self> {
        let Some(session) = sessions.for_folder(folder) else {
            anyhow::bail!(
                "nothing saved for {} — name a file, or press W here to remember one",
                folder.display()
            );
        };
        let mut tabs: Vec<Vec<App>> = Vec::with_capacity(session.tabs.len());
        let mut missing = Vec::new();
        // Where each saved tab ended up, so a join can find its sides even if
        // something before it failed to open and the positions shifted.
        let mut built: Vec<Option<usize>> = Vec::with_capacity(session.tabs.len());
        for saved in &session.tabs {
            // A join is made again from tabs already built — which is why they
            // are saved in order, and why a join of a join works.
            if let Some(join) = &saved.join {
                let sides = built
                    .get(join.left)
                    .copied()
                    .flatten()
                    .zip(built.get(join.right).copied().flatten());
                let made = sides.and_then(|(left, right)| {
                    let key = |tab: usize, name: &str| {
                        tabs.get(tab)?
                            .last()?
                            .data
                            .column_names
                            .iter()
                            .position(|n| n == name)
                    };
                    let left_key = key(left, &join.left_key)?;
                    let right_key = key(right, &join.right_key)?;
                    joined_view(&tabs, (left, left_key), (right, right_key))
                });
                match made {
                    Some(mut app) => {
                        app.apply_pattern(&saved.view);
                        built.push(Some(tabs.len()));
                        tabs.push(vec![app]);
                    }
                    None => {
                        missing.push("a join".to_string());
                        built.push(None);
                    }
                }
                continue;
            }
            let path = resolve_in(folder, &saved.file);
            // The session's own header reading, since it decides the schema.
            let header = App::header_from(&saved.view).unwrap_or(header);
            let dataset = Dataset::load_all(&path, header).ok().and_then(|sets| {
                // A workbook gives several sheets; take the one this tab held.
                sets.into_iter().find(|d| match &saved.sheet {
                    Some(sheet) => d.sheet() == Some(sheet.as_str()),
                    None => true,
                })
            });
            let Some(dataset) = dataset else {
                missing.push(saved.file.clone());
                built.push(None);
                continue;
            };
            let mut app = App::new(dataset);
            app.apply_pattern(&saved.view);
            built.push(Some(tabs.len()));
            tabs.push(vec![app]);
        }
        if tabs.is_empty() {
            anyhow::bail!(
                "none of the {} files in the session for {} could be opened",
                session.tabs.len(),
                folder.display()
            );
        }
        let current = built
            .get(session.current)
            .copied()
            .flatten()
            .unwrap_or(0)
            .min(tabs.len() - 1);
        let mut opened = Self {
            tabs,
            current,
            join: None,
            patterns,
            folder: folder.to_path_buf(),
            sessions: Sessions::default(),
        };
        // Transposed tabs are restored by transposing again, which is what the
        // view was in the first place.
        let transposed: Vec<usize> = session
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, saved)| saved.transposed)
            .filter_map(|(tab, _)| built.get(tab).copied().flatten())
            .collect();
        for tab in transposed {
            if let Some(stack) = opened.tabs.get(tab)
                && let Some(app) = stack.last()
                && let Ok(view) = transposed_view(app)
            {
                opened.tabs[tab].push(view);
            }
        }
        if !missing.is_empty() {
            opened.app_mut().status_msg =
                Some(format!("{} file(s) gone: {}", missing.len(), missing.join(", ")));
        }
        Ok(opened)
    }

    /// Remember every tab open here, so `lambris` alone reopens them.
    ///
    /// A joined tab is written down as the two tabs and key columns it was made
    /// from, since it holds no data of its own. Tabs are recorded in order and a
    /// join always points at earlier ones, so the whole lot — including a join
    /// of a join — rebuilds in a single pass.
    fn remember_session(&mut self) {
        let (session, lost) = self.snapshot_session();
        if session.tabs.is_empty() {
            self.app_mut().notice = Some(
                app::Notice::say("session", "there is nothing here that could be reopened")
                    .hint("a session remembers files and the joins made from them"),
            );
            return;
        }
        let count = session.tabs.len();
        self.sessions.put(session);
        let message = match self.sessions.save() {
            Ok(()) => {
                let note = match lost {
                    0 => String::new(),
                    n => format!(" · {n} left out"),
                };
                format!("remembered {count} tab(s) here{note}")
            }
            Err(e) => format!("session not saved: {e}"),
        };
        self.app_mut().status_msg = Some(message);
    }

    /// Whether what is open now differs from what was last saved for this
    /// folder — worked out by building what `W` would write and comparing it,
    /// so there is no flag to keep in step with the twenty things that change a
    /// view. `false` when the folder has no session: an ordinary run of the
    /// viewer has nothing to lose.
    ///
    /// Which tab is in front is left out of the comparison: looking at another
    /// tab is not work worth being asked about on the way out.
    fn session_changed(&self) -> bool {
        let Some(saved) = self.sessions.for_folder(&self.folder) else {
            return false;
        };
        let (now, _) = self.snapshot_session();
        now.tabs != saved.tabs
    }

    /// What `W` would write: every tab, and how many could not be described.
    fn snapshot_session(&self) -> (Session, usize) {
        let folder = self.folder.clone();
        let mut tabs = Vec::new();
        let mut lost = 0;
        for stack in &self.tabs {
            // The bottom of the stack is the table; anything above it is a view
            // of it, remembered as a flag rather than as a tab of its own.
            let Some(base) = stack.first() else { continue };
            let top = stack.last().unwrap_or(base);
            let view = base.pattern(String::new());
            let transposed = top.is_transposed;
            if base.is_file_backed() {
                tabs.push(SessionTab {
                    file: relative_to(&folder, &base.data.path),
                    sheet: base.data.sheet().map(str::to_string),
                    join: None,
                    transposed,
                    view,
                });
                continue;
            }
            // Not a file: the only other thing a tab can hold is a join, and
            // only one that still knows where it came from can be made again.
            match &base.origin {
                Some(origin) => tabs.push(SessionTab {
                    file: String::new(),
                    sheet: None,
                    join: Some(SavedJoin {
                        left: origin.left_tab,
                        right: origin.right_tab,
                        left_key: origin.left_key.clone(),
                        right_key: origin.right_key.clone(),
                    }),
                    transposed,
                    view,
                }),
                None => lost += 1,
            }
        }
        let current = self.current.min(tabs.len().saturating_sub(1));
        (
            Session {
                folder: folder.to_string_lossy().into_owned(),
                tabs,
                current,
            },
            lost,
        )
    }

    /// Arrange a freshly opened tab as its saved pattern says, if it has one.
    /// A sheet's pattern may ask for a different header reading than the file
    /// was opened with, in which case that one sheet is read again.
    fn apply_saved_pattern(&mut self, tab: usize) {
        let Some(app) = self.tabs.get(tab).and_then(|s| s.last()) else {
            return;
        };
        let (path, sheet) = (app.data.path.clone(), app.data.sheet().map(str::to_string));
        let Some(pattern) = self.patterns.matching(&path, sheet.as_deref()) else {
            return;
        };
        let pattern = pattern.clone();
        let bind = pattern.bind.clone();
        // Only a worksheet can disagree with the header the file was read with.
        if let (Some(wanted), Some(_)) = (App::header_from(&pattern), sheet.as_deref())
            && wanted != app.data.header
            && let Ok(reread) = app.data.reload_with_header(wanted)
        {
            self.tabs[tab] = vec![App::new(reread)];
        }
        let app = self.tabs[tab].last_mut().expect("the tab has a view");
        app.apply_pattern(&pattern);
        app.status_msg = Some(format!("pattern {bind}"));
    }

    /// Feed one key to the visible view, first telling it whether the wizard is
    /// running — which changes what `Enter` and `Esc` mean.
    fn key(&mut self, key: crossterm::event::KeyEvent) {
        let active = self.join.is_some();
        let app = self.app_mut();
        app.join_active = active;
        app.handle_key(key);
    }

    /// The line the wizard shows in place of the command hints.
    fn join_banner(&self) -> Option<String> {
        let wizard = self.join.as_ref()?;
        Some(match wizard.left {
            None => " join: go to the first key column — Tab switches tabs · Enter picks · Esc cancels".into(),
            Some((tab, col)) => format!(
                " join on {} — now the second key column · Enter joins · Esc cancels",
                self.column_label(tab, col).unwrap_or_else(|| "?".into()),
            ),
        })
    }

    /// `tab[sheet].column`, for naming a pick in the banner.
    fn column_label(&self, tab: usize, col: usize) -> Option<String> {
        let app = self.tabs.get(tab)?.last()?;
        let name = app.data.column_names.get(col)?;
        Some(format!("{}.{name}", app.data.label))
    }

    /// The view on top of the current tab's stack — what the user sees and
    /// types into.
    fn app_mut(&mut self) -> &mut App {
        self.tabs[self.current]
            .last_mut()
            .expect("every tab holds at least its base view")
    }

    /// Labels of the visible view of each tab, for the strip on the title line.
    fn strip(&self) -> ui::TabStrip {
        ui::TabStrip {
            labels: self
                .tabs
                .iter()
                .map(|stack| stack.last().expect("non-empty stack").data.label.clone())
                .collect(),
            current: self.current,
        }
    }

    /// Act on whatever the current view asked for on the last keypress.
    /// Returns `false` when the program should exit.
    fn step(&mut self) -> bool {
        let app = self.app_mut();
        // Taken before the rest, but acted on last: answering the question with
        // `y` sets both this and `save_session`, and the saving has to happen
        // on the way out rather than after it.
        let quitting = std::mem::take(&mut app.should_quit);
        // Drain the requests first so the tab set can be mutated freely below.
        let exit_transpose = std::mem::take(&mut app.exit_transpose);
        let transpose = std::mem::take(&mut app.transpose_request);
        let switch = app.switch_tab.take();
        let close = std::mem::take(&mut app.close_tab);
        let open = app.open_request.take();
        let toggle_header = std::mem::take(&mut app.toggle_header);
        let save_pattern = app.save_pattern.take();
        let save_session = std::mem::take(&mut app.save_session);
        let promote_header = std::mem::take(&mut app.promote_header);
        let join_request = std::mem::take(&mut app.join_request);
        let confirm = std::mem::take(&mut app.confirm);
        let cancel_join = std::mem::take(&mut app.cancel_join);

        if exit_transpose && self.tabs[self.current].len() > 1 {
            self.tabs[self.current].pop();
        }
        if transpose {
            let built = transposed_view(self.app_mut());
            match built {
                Ok(view) => self.tabs[self.current].push(view),
                Err(e) => self.app_mut().status_msg = Some(format!("transpose failed: {e}")),
            }
        }
        if toggle_header {
            // Stating it either way, so a `#` comment header is no longer what
            // decides: `T` is about the rows of the table.
            let header = self.app_mut().data.header;
            let named = !header.named();
            self.reload_header(
                HeaderSpec::At {
                    skip: header.skip(),
                    named,
                },
                if named { "first row: column names" } else { "first row: data" },
            );
        }
        if promote_header {
            self.promote_header_row();
        }
        if let Some(bind) = save_pattern {
            self.remember_pattern(bind);
        }
        if save_session {
            self.remember_session();
        }
        if join_request {
            self.join = Some(JoinWizard { left: None });
        }
        if cancel_join {
            self.join = None;
            self.app_mut().status_msg = Some("join cancelled".into());
        }
        if confirm {
            self.join_pick();
        }
        if let Some(delta) = switch {
            let n = self.tabs.len() as isize;
            self.current = (((self.current as isize + delta) % n + n) % n) as usize;
        }
        if let Some(path) = open {
            // Relative to the folder on screen, matching what `Tab` listed.
            let path = browse::resolve(&path, &self.app_mut().base_dir());
            let header = self.app_mut().data.header;
            let header = self
                .patterns
                .matching(&path, None)
                .and_then(App::header_from)
                .unwrap_or(header);
            match Dataset::load_all(&path, header) {
                // A workbook lands as several tabs; the first one becomes current.
                Ok(datasets) => {
                    let first = self.tabs.len();
                    self.tabs
                        .extend(datasets.into_iter().map(|d| vec![App::new(d)]));
                    self.current = first.min(self.tabs.len() - 1);
                    for tab in first..self.tabs.len() {
                        self.apply_saved_pattern(tab);
                    }
                }
                Err(e) => self.app_mut().status_msg = Some(format!("open failed: {e}")),
            }
        }
        if quitting {
            // Nothing was saved for this folder, or nothing has changed since:
            // leave without a word.
            if self.app_mut().quit_anyway || !self.session_changed() {
                return false;
            }
            let app = self.app_mut();
            app.quit_question = true;
            app.notice = Some(app::Notice::ask(
                "session",
                "this session has changed since it was saved",
                " y save · n discard · Esc stay ",
            ));
            return true;
        }
        if close {
            let closed = self.current;
            self.tabs.remove(closed);
            // Tab indices shift, so what a joined view remembers about where it
            // came from has to move with them — or be dropped when a side goes,
            // since it could no longer be made again.
            for stack in &mut self.tabs {
                for app in stack.iter_mut() {
                    let Some(origin) = &mut app.origin else { continue };
                    let gone = origin.left_tab == closed || origin.right_tab == closed;
                    if gone {
                        app.origin = None;
                        continue;
                    }
                    if origin.left_tab > closed {
                        origin.left_tab -= 1;
                    }
                    if origin.right_tab > closed {
                        origin.right_tab -= 1;
                    }
                }
            }
            // A pending join pick has to move with them too.
            let orphaned = match self.join.as_mut().and_then(|w| w.left.as_mut()) {
                Some((tab, _)) if *tab == closed => true,
                Some((tab, _)) if *tab > closed => {
                    *tab -= 1;
                    false
                }
                _ => false,
            };
            if self.tabs.is_empty() {
                return false; // closed the last tab
            }
            self.current = self.current.min(self.tabs.len() - 1);
            if orphaned {
                self.join = None;
                self.app_mut().status_msg =
                    Some("join cancelled: that tab was closed".into());
            }
        }
        true
    }
}

impl Tabs {
    /// Re-read the current tab's file with a different reading of its top rows.
    /// The schema changes with it (the names, and the type of any column the
    /// header row joins), so the tab starts from a fresh view rather than
    /// trying to carry a cursor, filter or sort across the change.
    fn reload_header(&mut self, want: HeaderSpec, msg: &str) {
        let app = self.app_mut();
        match app.data.reload_with_header(want) {
            Ok(dataset) => {
                let mut view = App::new(dataset);
                view.status_msg = Some(msg.to_string());
                self.tabs[self.current] = vec![view];
            }
            Err(e) => app.status_msg = Some(format!("header: {e}")),
        }
    }

    /// Make the selected row the header, dropping everything above it — the fix
    /// for a file that puts title or provenance rows before its real header.
    /// Pressing `H` again with a promoted header puts it back at the top.
    fn promote_header_row(&mut self) {
        let app = self.app_mut();
        // Any stated header row means `H` has been used, so this press takes it
        // back — testing `skip > 0` would miss the case where the row wanted is
        // the first one, which is exactly what a `#`-commented file needs.
        if matches!(app.data.header, HeaderSpec::At { named: true, .. }) {
            self.reload_header(HeaderSpec::Auto, "header: back to how the file reads");
            return;
        }
        if app.row_count() == 0 {
            return;
        }
        // The raw file row under the cursor, which is what becomes the header.
        let skip = app.data.raw_row(app.selected_orig());
        let spec = HeaderSpec::At { skip, named: true };
        let msg = format!("header: row {}", spec.header_line());
        self.reload_header(spec, &msg);
    }
}

impl Tabs {
    /// Save the current view's arrangement under `bind`, or forget the saved
    /// one when `bind` is empty.
    fn remember_pattern(&mut self, bind: String) {
        let app = self.app_mut();
        if !app.is_file_backed() {
            app.status_msg =
                Some("a pattern belongs to a file — this view is not one".into());
            return;
        }
        let sheet = app.data.sheet().map(str::to_string);
        if bind.is_empty() {
            let previous = app.pattern_bind();
            let forgotten = self.patterns.forget(&previous, sheet.as_deref());
            let message = match (forgotten, self.patterns.save()) {
                (_, Err(e)) => format!("pattern not saved: {e}"),
                (true, _) => format!("forgot the pattern for {previous}"),
                (false, _) => format!("no pattern saved for {previous}"),
            };
            self.app_mut().status_msg = Some(message);
            return;
        }
        let pattern = app.pattern(bind.clone());
        self.patterns.put(pattern);
        let message = match self.patterns.save() {
            Ok(()) => format!("remembered for {bind}"),
            Err(e) => format!("pattern not saved: {e}"),
        };
        self.app_mut().status_msg = Some(message);
    }

    /// Record the key column under the cursor: the first press stores it, the
    /// second runs the join.
    fn join_pick(&mut self) {
        let pick = (self.current, self.app_mut().selected_col());
        let Some(wizard) = &mut self.join else { return };
        match wizard.left {
            None => {
                wizard.left = Some(pick);
                let named = self.column_label(pick.0, pick.1).unwrap_or_default();
                self.app_mut().status_msg = Some(format!("join on {named} …"));
            }
            Some(left) => {
                self.join = None;
                self.run_join(left, pick);
            }
        }
    }

    /// Join the two picked columns and open the result in a new tab. Each side
    /// contributes the rows it is currently showing, so filters, sorts and
    /// transposed views all carry through.
    fn run_join(&mut self, left: (usize, usize), right: (usize, usize)) {
        // A tab could have been closed between the two picks.
        if self.tabs.get(left.0).is_none() || self.tabs.get(right.0).is_none() {
            self.app_mut().status_msg = Some("join: that tab is gone".into());
            return;
        }
        match joined_view(&self.tabs, left, right) {
            Some(view) => {
                self.tabs.push(vec![view]);
                self.current = self.tabs.len() - 1;
            }
            None if interrupt::take() => {
                self.app_mut().status_msg = Some("join cancelled".into())
            }
            None => self.app_mut().status_msg = Some("join failed".into()),
        }
    }
}

/// Build the view a join produces, without deciding where it goes — so the
/// wizard and a reopening session make one the same way.
fn joined_view(
    tabs: &[Vec<App>],
    left: (usize, usize),
    right: (usize, usize),
) -> Option<App> {
    let ((left_tab, left_col), (right_tab, right_col)) = (left, right);
    let left_app = tabs.get(left_tab)?.last()?;
    let right_app = tabs.get(right_tab)?.last()?;
    let left_rows = left_app.view_rows(JOIN_MAX_ROWS);
    let right_rows = right_app.view_rows(JOIN_MAX_ROWS);
    let joined = Dataset::join(
        JoinSide {
            data: &left_app.data,
            rows: &left_rows,
            cols: left_app.visible_cols(),
            key: left_col,
        },
        JoinSide {
            data: &right_app.data,
            rows: &right_rows,
            cols: right_app.visible_cols(),
            key: right_col,
        },
        interrupt::requested,
    );
    let (dataset, report) = joined.ok()??;
    let mut view = App::new(dataset);
    // Where it came from, so a session can make it again.
    view.origin = Some(app::JoinOrigin {
        left_tab,
        right_tab,
        left_key: left_app.data.column_names[left_col].clone(),
        right_key: right_app.data.column_names[right_col].clone(),
    });
    let mut msg = format!(
        "{} rows · {} matched, {} unmatched",
        report.rows, report.matched, report.unmatched
    );
    if report.truncated {
        msg.push_str(&format!(" · cut at {JOIN_MAX_ROWS}"));
    }
    view.status_msg = Some(msg);
    Some(view)
}

/// Draw the current tab's top view, feed it one key, then let [`Tabs::step`]
/// apply whatever it asked for. Transposed views and freshly opened files run
/// through this very same path, so every command works unchanged.
fn run(terminal: &mut ratatui::DefaultTerminal, tabs: &mut Tabs) -> Result<()> {
    loop {
        let strip = tabs.strip();
        let banner = tabs.join_banner();
        let app = tabs.app_mut();
        terminal
            .draw(|frame| {
                if let Err(e) = ui::render(frame, app, &strip, banner.as_deref()) {
                    app.render_error = Some(e.to_string());
                    app.should_quit = true;
                }
            })
            .context("drawing frame")?;

        if let Some(e) = app.render_error.take() {
            anyhow::bail!("render error: {e}");
        }
        if let Event::Key(key) = event::read().context("reading input event")?
            && key.kind == KeyEventKind::Press
        {
            tabs.key(key);
        }
        if !tabs.step() {
            break;
        }
    }
    Ok(())
}

/// Build a transposed view from the current app's view, capped in width.
fn transposed_view(app: &App) -> Result<App> {
    if app.visible_cols().len() < 2 || app.row_count() == 0 {
        anyhow::bail!("needs at least 2 columns and 1 row");
    }
    let rows = app.view_rows(TRANSPOSE_MAX_RECORDS);
    let dataset = app.data.transpose(&rows, app.visible_cols())?;
    let mut view = App::new(dataset);
    view.is_transposed = true;
    if app.row_count() > TRANSPOSE_MAX_RECORDS {
        view.status_msg = Some(format!(
            "showing first {TRANSPOSE_MAX_RECORDS} of {} records",
            app.row_count()
        ));
    }
    Ok(view)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use arrow::array::{Int64Array, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::io::Write;
    use std::time::{Duration, Instant};
    use parquet::arrow::ArrowWriter;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Write a small parquet fixture to a temp path and return it.
    /// Column `score` is nullable with every 3rd value null.
    fn fixture() -> PathBuf {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, false),
            Field::new("score", DataType::Int64, true),
        ]));
        let ids = Int64Array::from((0..50).collect::<Vec<_>>());
        let names = StringArray::from(
            (0..50).map(|i| format!("item_{i:04}")).collect::<Vec<_>>(),
        );
        let scores = Int64Array::from(
            (0..50)
                .map(|i| if i % 3 == 0 { None } else { Some(i * 2) })
                .collect::<Vec<_>>(),
        );
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(ids), Arc::new(names), Arc::new(scores)],
        )
        .unwrap();

        // Unique per call so parallel tests never read a half-written file.
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("lambris_test_fixture_{n}.parquet"));
        let file = std::fs::File::create(&path).unwrap();
        let mut writer = ArrowWriter::try_new(file, schema, None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        path
    }

    /// Write `content` to a uniquely-named temp file with the given extension.
    fn write_text_fixture(ext: &str, content: &str) -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("lambris_test_text_{n}.{ext}"));
        std::fs::write(&path, content).unwrap();
        path
    }

    /// Write an `n`-row parquet fixture (`id`, `name`) to a unique temp path.
    fn big_parquet(n: i64) -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, false),
        ]));
        let ids = Int64Array::from((0..n).collect::<Vec<_>>());
        let names = StringArray::from((0..n).map(|i| format!("r{i}")).collect::<Vec<_>>());
        let batch =
            RecordBatch::try_new(schema.clone(), vec![Arc::new(ids), Arc::new(names)]).unwrap();
        let k = SEQ.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("lambris_test_big_{k}.parquet"));
        let file = std::fs::File::create(&path).unwrap();
        let mut writer = ArrowWriter::try_new(file, schema, None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        path
    }

    /// Write an `n`-row CSV fixture (`id,name`) to a unique temp path.
    fn big_csv(n: usize) -> PathBuf {
        let mut s = String::from("id,name\n");
        for i in 0..n {
            s.push_str(&format!("{i},r{i}\n"));
        }
        write_text_fixture("csv", &s)
    }

    /// Write a three-sheet xlsx workbook to a unique temp path.
    /// `Numbers` has typed columns plus a blank cell and a `#DIV/0!` error,
    /// `Dates` a pure-date column and one carrying a time of day, and `Blank`
    /// is left completely empty.
    fn xlsx_fixture() -> PathBuf {
        use rust_xlsxwriter::{ExcelDateTime, Format, Formula, Workbook};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let mut workbook = Workbook::new();
        let sheet = workbook.add_worksheet();
        sheet.set_name("Numbers").unwrap();
        for (c, name) in ["id", "name", "score"].iter().enumerate() {
            sheet.write_string(0, c as u16, *name).unwrap();
        }
        for (i, (id, name, score)) in [(1, "alpha", 3.5), (2, "beta", 10.25), (3, "gamma", 0.5)]
            .iter()
            .enumerate()
        {
            let r = i as u32 + 1;
            sheet.write_number(r, 0, *id as f64).unwrap();
            sheet.write_string(r, 1, *name).unwrap();
            sheet.write_number(r, 2, *score).unwrap();
        }
        // Fourth row: the name cell is left blank and the score is an Excel error.
        sheet.write_number(4, 0, 4.0).unwrap();
        sheet
            .write_formula(4, 2, Formula::new("=1/0").set_result("#DIV/0!"))
            .unwrap();

        let day_fmt = Format::new().set_num_format("yyyy\\-mm\\-dd");
        let moment_fmt = Format::new().set_num_format("yyyy\\-mm\\-dd\\ hh:mm");
        let sheet = workbook.add_worksheet();
        sheet.set_name("Dates").unwrap();
        sheet.write_string(0, 0, "day").unwrap();
        sheet.write_string(0, 1, "moment").unwrap();
        for (i, day) in [31u8, 1].iter().enumerate() {
            let r = i as u32 + 1;
            let month = if i == 0 { 1 } else { 2 };
            sheet
                .write_datetime_with_format(
                    r,
                    0,
                    ExcelDateTime::from_ymd(2024, month, *day).unwrap(),
                    &day_fmt,
                )
                .unwrap();
            sheet
                .write_datetime_with_format(
                    r,
                    1,
                    ExcelDateTime::from_ymd(2024, month, *day)
                        .unwrap()
                        .and_hms(12, 30, 0)
                        .unwrap(),
                    &moment_fmt,
                )
                .unwrap();
        }
        workbook.add_worksheet().set_name("Blank").unwrap();

        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("lambris_test_book_{n}.xlsx"));
        workbook.save(&path).unwrap();
        path
    }

    /// A small directory tree to browse: a subfolder with one file inside, two
    /// files beside it, and a hidden one. Returns the root.
    fn browse_fixture() -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let mut root = std::env::temp_dir();
        root.push(format!("lambris_test_browse_{n}"));
        // A directory, unlike the file fixtures, would otherwise accumulate
        // entries from earlier runs and change what the picker offers.
        let _ = std::fs::remove_dir_all(&root);
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(root.join("alpha.csv"), "a,b\n1,2\n").unwrap();
        std::fs::write(root.join("beta.tsv"), "a\tb\n1\t2\n").unwrap();
        std::fs::write(root.join(".hidden.csv"), "a,b\n9,9\n").unwrap();
        std::fs::write(nested.join("inner.csv"), "x,y\n3,4\n").unwrap();
        // The picker canonicalises, so tests compare against canonical paths.
        std::fs::canonicalize(&root).unwrap()
    }

    /// Labels of the entries the picker is offering.
    fn offered(app: &App) -> Vec<String> {
        app.completions
            .as_ref()
            .map(|c| c.entries.iter().map(|e| e.label()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn open_prompt_lists_the_current_files_folder() {
        let root = browse_fixture();
        let mut tabs =
            Tabs::open(&[root.join("alpha.csv")], HeaderSpec::default(), Store::default()).unwrap();

        // Before Tab it is a plain prompt.
        tabs.app_mut().handle_key(key('o'));
        assert!(tabs.app_mut().completions.is_none());

        // Tab lists the folder the open file lives in: directories first,
        // dotfiles hidden until asked for.
        tabs.app_mut().handle_key(code(KeyCode::Tab));
        assert_eq!(
            offered(tabs.app_mut()),
            vec!["../", "nested/", "alpha.csv", "beta.tsv"],
            "`..` leads, then directories, then files"
        );

        // The listing is drawn over the table, headed by the folder.
        let text = buffer_text(tabs.app_mut(), 90, 20);
        assert!(text.contains("nested/"), "picker missing: {text}");
        assert!(text.contains("Tab: list folder"), "legend missing: {text}");
        let leaf = root.file_name().unwrap().to_string_lossy().into_owned();
        assert!(text.contains(&leaf), "folder name missing: {text}");
        // Narrow: the title keeps the deep end of the path, not its head.
        let text = buffer_text(tabs.app_mut(), 30, 20);
        assert!(text.contains('…') && text.contains(&leaf[leaf.len() - 6..]));

        // Tab again walks the list; arrows do too, and it wraps.
        tabs.app_mut().handle_key(code(KeyCode::Tab));
        assert_eq!(tabs.app_mut().completions.as_ref().unwrap().selected, 1);
        tabs.app_mut().handle_key(code(KeyCode::Up));
        assert_eq!(tabs.app_mut().completions.as_ref().unwrap().selected, 0);
        tabs.app_mut().handle_key(code(KeyCode::Up));
        assert_eq!(
            tabs.app_mut().completions.as_ref().unwrap().selected,
            3,
            "wraps to the last entry"
        );
    }

    #[test]
    fn open_prompt_steps_into_a_folder_and_opens_a_file() {
        let root = browse_fixture();
        let mut tabs =
            Tabs::open(&[root.join("alpha.csv")], HeaderSpec::default(), Store::default()).unwrap();
        tabs.app_mut().handle_key(key('o'));
        tabs.app_mut().handle_key(code(KeyCode::Tab));

        // Enter on `nested/` steps inside and lists it, rather than opening it.
        tabs.app_mut().handle_key(code(KeyCode::Down)); // past `..`
        tabs.app_mut().handle_key(code(KeyCode::Enter));
        assert!(tabs.step());
        assert_eq!(tabs.tabs.len(), 1, "a folder is not a file to open");
        assert!(tabs.app_mut().input.ends_with("nested/"));
        assert_eq!(offered(tabs.app_mut()), vec!["../", "inner.csv"]);

        // Enter on the file opens it in a new tab.
        tabs.app_mut().handle_key(code(KeyCode::Down)); // past `..`
        tabs.app_mut().handle_key(code(KeyCode::Enter));
        assert!(tabs.step());
        assert_eq!(tabs.tabs.len(), 2);
        assert_eq!(tabs.current, 1);
        assert_eq!(tabs.app_mut().data.label, "inner.csv");
        assert_eq!(tabs.app_mut().data.column_names, vec!["x", "y"]);
        assert!(tabs.app_mut().completions.is_none(), "picker closed on open");
    }

    #[test]
    fn open_prompt_completes_and_filters_as_you_type() {
        let root = browse_fixture();
        let mut tabs =
            Tabs::open(&[root.join("alpha.csv")], HeaderSpec::default(), Store::default()).unwrap();

        // A bare prefix narrows the same folder — Tab finishes a lone match.
        tabs.app_mut().handle_key(key('o'));
        type_str(tabs.app_mut(), "bet");
        tabs.app_mut().handle_key(code(KeyCode::Tab));
        assert!(
            tabs.app_mut().completions.is_none(),
            "a single file is a finished answer"
        );
        assert_eq!(tabs.app_mut().input, root.join("beta.tsv").to_string_lossy());
        tabs.app_mut().handle_key(code(KeyCode::Enter));
        assert!(tabs.step());
        assert_eq!(tabs.app_mut().data.label, "beta.tsv");

        // Typing with the picker up filters it, and Backspace widens it again.
        tabs.app_mut().handle_key(key('o'));
        tabs.app_mut().handle_key(code(KeyCode::Tab));
        type_str(tabs.app_mut(), "n");
        assert_eq!(offered(tabs.app_mut()), vec!["nested/"]);
        tabs.app_mut().handle_key(code(KeyCode::Backspace));
        assert_eq!(offered(tabs.app_mut()).len(), 4);

        // A prefix matching nothing says so instead of closing.
        type_str(tabs.app_mut(), "zzz");
        assert!(offered(tabs.app_mut()).is_empty());
        let note = tabs
            .app_mut()
            .completions
            .as_ref()
            .unwrap()
            .note
            .clone()
            .unwrap_or_default();
        assert!(note.contains("nothing starting with"), "unexpected: {note}");
    }

    #[test]
    fn slash_types_a_path_separator_not_a_search() {
        let root = browse_fixture();
        let mut tabs =
            Tabs::open(&[root.join("alpha.csv")], HeaderSpec::default(), Store::default()).unwrap();

        // In the open prompt `/` is a path separator: it reaches the input and
        // does not start a search.
        tabs.app_mut().handle_key(key('o'));
        type_str(tabs.app_mut(), "nested/");
        assert_eq!(tabs.app_mut().input, "nested/");
        assert!(
            matches!(tabs.app_mut().mode, app::Mode::Input(app::InputKind::Open)),
            "still the open prompt"
        );
        assert!(tabs.app_mut().search.is_none(), "no search was started");
        // And it navigates: the listing is of that folder.
        tabs.app_mut().handle_key(code(KeyCode::Tab));
        assert!(offered(tabs.app_mut()).contains(&"inner.csv".to_string()));

        // Back in the table, `/` still opens search as always.
        tabs.app_mut().handle_key(code(KeyCode::Esc)); // close the picker
        tabs.app_mut().handle_key(code(KeyCode::Esc)); // leave the prompt
        tabs.app_mut().handle_key(key('/'));
        assert!(matches!(
            tabs.app_mut().mode,
            app::Mode::Input(app::InputKind::Search)
        ));
    }

    #[test]
    fn open_prompt_goes_up_a_folder_and_opens_from_there() {
        let root = browse_fixture();
        let mut tabs = Tabs::open(
            &[root.join("nested").join("inner.csv")],
            HeaderSpec::default(),
            Store::default(),
        )
        .unwrap();
        tabs.app_mut().handle_key(key('o'));
        tabs.app_mut().handle_key(code(KeyCode::Tab));
        // Starting in `nested/`, the way up is the first entry.
        assert_eq!(offered(tabs.app_mut())[0], "../");

        tabs.app_mut().handle_key(code(KeyCode::Enter));
        assert!(tabs.step());
        // The typed path is the parent itself — no `..` left in it.
        let input = tabs.app_mut().input.clone();
        assert_eq!(input, format!("{}/", root.display()));
        assert!(!input.contains(".."), "path should be plain: {input}");
        assert!(offered(tabs.app_mut()).contains(&"beta.tsv".to_string()));

        // And a file picked up there actually opens.
        while tabs.app_mut().completions.as_ref().unwrap().selected_entry().unwrap().name
            != "beta.tsv"
        {
            tabs.app_mut().handle_key(code(KeyCode::Down));
        }
        tabs.app_mut().handle_key(code(KeyCode::Enter));
        assert!(tabs.step());
        assert_eq!(tabs.tabs.len(), 2, "status: {:?}", tabs.app_mut().status_msg);
        assert_eq!(tabs.app_mut().data.label, "beta.tsv");
    }

    #[test]
    fn open_prompt_shows_hidden_files_only_when_asked() {
        let root = browse_fixture();
        let mut tabs =
            Tabs::open(&[root.join("alpha.csv")], HeaderSpec::default(), Store::default()).unwrap();
        tabs.app_mut().handle_key(key('o'));
        tabs.app_mut().handle_key(code(KeyCode::Tab));
        assert!(!offered(tabs.app_mut()).contains(&".hidden.csv".to_string()));
        type_str(tabs.app_mut(), ".");
        assert_eq!(offered(tabs.app_mut()), vec!["../", ".hidden.csv"]);
    }

    #[test]
    fn open_prompt_esc_closes_the_picker_before_the_prompt() {
        let root = browse_fixture();
        let mut tabs =
            Tabs::open(&[root.join("alpha.csv")], HeaderSpec::default(), Store::default()).unwrap();
        tabs.app_mut().handle_key(key('o'));
        tabs.app_mut().handle_key(code(KeyCode::Tab));
        assert!(tabs.app_mut().completions.is_some());

        // First Esc puts the listing away but keeps what was typed.
        tabs.app_mut().handle_key(code(KeyCode::Esc));
        assert!(tabs.app_mut().completions.is_none());
        assert!(matches!(tabs.app_mut().mode, app::Mode::Input(_)));

        // The second leaves the prompt without opening anything.
        tabs.app_mut().handle_key(code(KeyCode::Esc));
        assert!(matches!(tabs.app_mut().mode, app::Mode::Normal));
        assert!(tabs.step());
        assert_eq!(tabs.tabs.len(), 1);
    }

    #[test]
    fn picker_scrolls_to_keep_the_highlight_visible() {
        // More entries than the box shows: the window follows the highlight.
        let root = browse_fixture();
        for i in 0..20 {
            std::fs::write(root.join(format!("f{i:02}.csv")), "a\n1\n").unwrap();
        }
        let listing = browse::Completions::for_input("", &root);
        assert!(listing.entries.len() > browse::VISIBLE);
        let (start, window) = listing.window(browse::VISIBLE);
        assert_eq!((start, window.len()), (0, browse::VISIBLE));

        let mut listing = listing;
        let last = listing.entries.len() - 1;
        listing.step(-1); // wrap to the end
        assert_eq!(listing.selected, last);
        let (start, window) = listing.window(browse::VISIBLE);
        assert_eq!(start + window.len() - 1, last, "the last entry is on screen");
    }

    /// Two related tables: `meta` keyed by sample, `dict` describing each one.
    fn join_fixtures() -> (PathBuf, PathBuf) {
        let meta = write_text_fixture("csv", "sample,depth\nS1,10\nS2,20\nS3,30\n");
        let dict = write_text_fixture("csv", "sample,label\nS1,control\nS2,treated\n");
        (meta, dict)
    }

    /// Drive the wizard: `J`, then Enter on `left_col` of the current tab, then
    /// move to `right_tab` and Enter on `right_col`.
    fn run_wizard(tabs: &mut Tabs, left_col: usize, right_tab: usize, right_col: usize) {
        tabs.key(key('J'));
        assert!(tabs.step());
        tabs.app_mut().selected_pos = left_col;
        tabs.key(KeyEvent::from(KeyCode::Enter));
        assert!(tabs.step());
        while tabs.current != right_tab {
            tabs.key(KeyEvent::from(KeyCode::Tab));
            assert!(tabs.step());
        }
        tabs.app_mut().selected_pos = right_col;
        tabs.key(KeyEvent::from(KeyCode::Enter));
        assert!(tabs.step());
    }

    /// Walk the `S` wizard to the slice `start..end` (0-based, end exclusive),
    /// then pick `method`. The end edge opens at the far side of the field, so
    /// it is walked back rather than forward.
    fn key_sort_wizard(app: &mut App, start: usize, end: usize, method: char) {
        app.handle_key(key('S'));
        for _ in 0..start {
            app.handle_key(code(KeyCode::Right));
        }
        app.handle_key(code(KeyCode::Enter));
        let width = app.key_sort.expect("wizard running").width;
        for _ in 0..width.saturating_sub(end) {
            app.handle_key(code(KeyCode::Left));
        }
        app.handle_key(code(KeyCode::Enter));
        app.handle_key(key(method));
    }

    /// The column's values in view order.
    fn column_values(app: &App, col: usize) -> Vec<String> {
        app.data
            .cells(col, &app.view_rows(usize::MAX))
            .unwrap()
            .into_iter()
            .map(|v| v.unwrap_or_default())
            .collect()
    }

    #[test]
    fn key_sort_sorts_by_a_slice_of_the_field() {
        // Characters 10-11 hold the number the rows should order by.
        let csv = "id\nSMP_2024_07_a\nSMP_2024_03_b\nSMP_2024_11_c\n";
        let mut app = App::new(Dataset::load(&write_text_fixture("csv", csv)).unwrap());
        key_sort_wizard(&mut app, 9, 11, 'n');

        assert_eq!(
            column_values(&app, 0),
            vec!["SMP_2024_03_b", "SMP_2024_07_a", "SMP_2024_11_c"]
        );
        let spec = app.sort.expect("sorted");
        let slice = spec.key.expect("by a slice");
        assert_eq!((slice.start, slice.end), (9, 11), "0-based, end exclusive");
        assert_eq!(slice.method, data::SortMethod::Numeric);
        assert!(app.key_sort.is_none(), "the wizard is done");

        // `s` then cycles the direction, keeping the slice.
        app.handle_key(key('s'));
        let spec = app.sort.expect("still sorted");
        assert_eq!(spec.dir, app::SortDir::Desc);
        assert_eq!(spec.key.map(|k| (k.start, k.end)), Some((9, 11)));
        assert_eq!(
            column_values(&app, 0),
            vec!["SMP_2024_11_c", "SMP_2024_07_a", "SMP_2024_03_b"]
        );
    }

    #[test]
    fn key_sort_shows_the_same_slice_in_every_row() {
        // The point of picking it visually: one highlight per row, at the same
        // offsets, and none at all where the value is too short to reach them.
        let csv = "id\nSMP_2024_07_a\nSMP_2024_03_b\nSHORT\n";
        let mut app = App::new(Dataset::load(&write_text_fixture("csv", csv)).unwrap());
        app.handle_key(key('S'));
        for _ in 0..9 {
            app.handle_key(code(KeyCode::Right));
        }
        app.handle_key(code(KeyCode::Enter));
        // The end opens at the field's far edge; walk it back to char 11.
        let width = app.key_sort.unwrap().width;
        for _ in 0..width - 11 {
            app.handle_key(code(KeyCode::Left));
        }

        let mut terminal = Terminal::new(TestBackend::new(40, 7)).unwrap();
        terminal
            .draw(|f| ui::render(f, &mut app, &ui::TabStrip::default(), None).unwrap())
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let highlighted = |y: u16| -> String {
            (0..24u16)
                .filter(|&x| buf.cell((x, y)).unwrap().bg == ratatui::style::Color::Cyan)
                .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                .collect()
        };
        assert_eq!(highlighted(2), "07", "first row's slice");
        assert_eq!(highlighted(3), "03", "and the second's, at the same offsets");
        assert_eq!(highlighted(4), "", "SHORT never reaches char 10");

        // The prompt says which stage and which characters.
        let text = buffer_text(&mut app, 80, 10);
        assert!(text.contains("chars 10-11"), "no offsets shown: {text}");
        assert!(text.contains("end of the sort key"), "no stage shown: {text}");
    }

    #[test]
    fn key_sort_takes_the_whole_field_by_default() {
        // `S`, Enter, Enter, v — no arrows at all — sorts by the whole field,
        // which is what a natural sort usually wants.
        let csv = "chrom\nchr10\nchr2\nchr1\n";
        let mut app = App::new(Dataset::load(&write_text_fixture("csv", csv)).unwrap());
        app.handle_key(key('S'));
        app.handle_key(code(KeyCode::Enter));
        let wizard = app.key_sort.expect("wizard running");
        assert_eq!(
            (wizard.start, wizard.end),
            (0, 5),
            "the end opens at the far edge of the widest value"
        );
        app.handle_key(code(KeyCode::Enter));
        app.handle_key(key('v'));

        assert_eq!(column_values(&app, 0), vec!["chr1", "chr2", "chr10"]);
        let slice = app.sort.unwrap().key.unwrap();
        assert_eq!((slice.start, slice.end), (0, 5));
    }

    #[test]
    fn key_sort_methods_order_differently() {
        // `chr2` vs `chr10`: alphabetic puts 10 first, natural gets it right,
        // and numeric can't parse either so the order is left alone.
        let csv = "chrom\nchr10\nchr2\nchr1\n";
        let path = write_text_fixture("csv", csv);
        let full = 5; // the whole field

        let mut abc = App::new(Dataset::load(&path).unwrap());
        key_sort_wizard(&mut abc, 0, full, 'a');
        assert_eq!(column_values(&abc, 0), vec!["chr1", "chr10", "chr2"]);

        let mut nat = App::new(Dataset::load(&path).unwrap());
        key_sort_wizard(&mut nat, 0, full, 'v');
        assert_eq!(column_values(&nat, 0), vec!["chr1", "chr2", "chr10"]);

        let mut num = App::new(Dataset::load(&path).unwrap());
        key_sort_wizard(&mut num, 0, full, 'n');
        assert_eq!(
            column_values(&num, 0),
            vec!["chr10", "chr2", "chr1"],
            "nothing parses, so the stable sort keeps the original order"
        );
    }

    #[test]
    fn key_sort_puts_rows_without_the_slice_last() {
        let csv = "id\nAA_9\nBB\nCC_1\n";
        let mut app = App::new(Dataset::load(&write_text_fixture("csv", csv)).unwrap());
        // Characters 4-4: `BB` never reaches it.
        key_sort_wizard(&mut app, 3, 4, 'n');
        assert_eq!(column_values(&app, 0), vec!["CC_1", "AA_9", "BB"]);
    }

    #[test]
    fn key_sort_can_be_cancelled_and_its_edges_clamp() {
        let csv = "id\nabcd\nefgh\n";
        let mut app = App::new(Dataset::load(&write_text_fixture("csv", csv)).unwrap());

        // Esc leaves the sort untouched.
        app.handle_key(key('S'));
        assert!(app.key_sort.is_some());
        app.handle_key(code(KeyCode::Esc));
        assert!(app.key_sort.is_none() && app.sort.is_none());
        assert_eq!(app.status_msg.as_deref(), Some("sort key cancelled"));

        // The start stops at the last character of the widest value …
        app.handle_key(key('S'));
        for _ in 0..20 {
            app.handle_key(code(KeyCode::Right));
        }
        assert_eq!(app.key_sort.unwrap().start, 3, "clamped to the width");
        // … and the end never crosses back over the start.
        app.handle_key(code(KeyCode::Enter));
        for _ in 0..5 {
            app.handle_key(code(KeyCode::Left));
        }
        let wizard = app.key_sort.unwrap();
        assert_eq!((wizard.start, wizard.end), (3, 4), "at least one character");

        // Scrolling while choosing is allowed — that is how you check the
        // offsets against other values — and leaves the wizard alone.
        app.handle_key(key('j'));
        assert_eq!(app.selected_row, 1);
        assert!(app.key_sort.is_some());
    }

    #[test]
    fn key_sort_declines_when_there_is_nothing_to_slice() {
        let csv = "id,note\n1,\n2,\n";
        let mut app = App::new(Dataset::load(&write_text_fixture("csv", csv)).unwrap());
        app.selected_pos = 1; // the empty column
        app.handle_key(key('S'));
        assert!(app.key_sort.is_none());
        assert_eq!(
            app.status_msg.as_deref(),
            Some("nothing to slice in this column")
        );
    }

    /// A pattern file of its own, so nothing here touches a real config.
    fn store_path() -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("lambris_test_patterns_{n}.json"));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// Save the current view's pattern under `bind` (empty forgets it).
    fn save_pattern(tabs: &mut Tabs, bind: &str) {
        tabs.key(key('w'));
        // The prompt opens pre-filled with the file's name; replace it.
        let filled = tabs.app_mut().input.clone();
        for _ in 0..filled.chars().count() {
            tabs.key(KeyEvent::from(KeyCode::Backspace));
        }
        type_str(tabs.app_mut(), bind);
        tabs.key(KeyEvent::from(KeyCode::Enter));
        assert!(tabs.step());
    }

    /// Press `=` the way a person taps it: far enough apart not to look like a
    /// held key. `tick` has to keep counting up across calls, or successive
    /// taps land at the same moment and are read as a hold.
    fn tap_summary(app: &mut App, origin: Instant, tick: &mut u64) {
        *tick += 1;
        app.handle_key_at(key('='), origin + Duration::from_millis(*tick * 400));
    }

    /// The summary line as drawn, trailing blanks trimmed.
    fn summary_line(app: &mut App, w: u16, h: u16) -> String {
        // It is the last line of the body: title, header, rows, summary.
        buffer_line(app, w, h, h - 3)
    }

    #[test]
    fn the_summary_line_totals_and_averages() {
        // `depth` is read on a log scale, where a total means little.
        let csv = "sample,reads,depth\nS1,1000,10.5\nS2,3000,20.25\nS3,2000,\n";
        let mut app = App::new(Dataset::load(&write_text_fixture("csv", csv)).unwrap());
        assert!(summary_line(&mut app, 60, 9).trim().is_empty(), "off to begin with");
        let (origin, mut tick) = (Instant::now(), 0);

        app.selected_pos = 2;
        app.handle_key(key('%')); // log styling on depth
        app.selected_pos = 1; // and cycle `reads`
        tap_summary(&mut app, origin, &mut tick);
        assert_eq!(app.summary, Some(app::Summary::Auto));
        // Auto totals the plain column and averages the log one.
        let line = summary_line(&mut app, 60, 9);
        assert!(line.contains("6000"), "reads should be totalled: {line}");
        assert!(line.contains("15.38"), "depth should be averaged: {line}");
        assert!(line.starts_with("Σμ"), "the gutter marks the mode: {line}");
        // A non-numeric column is left blank, and a null is simply not counted.
        assert!(!line.contains("S1"));

        // Cycling moves the *selected* column on and leaves the rest alone.
        for _ in 0..3 {
            tap_summary(&mut app, origin, &mut tick);
        }
        assert_eq!(app.summary_at(1), Some(app::Summary::Stddev));
        assert_eq!(app.summary_at(2), Some(app::Summary::Auto), "depth untouched");
        let line = summary_line(&mut app, 60, 9);
        assert!(line.contains("1000"), "reads' spread: {line}");
        assert!(line.contains("15.38"), "depth still averaged: {line}");

        // `mean±sd` is given room rather than clipped to something misleading.
        tap_summary(&mut app, origin, &mut tick);
        let line = summary_line(&mut app, 60, 9);
        assert!(line.contains("2000±1000"), "mean and spread together: {line}");
        tap_summary(&mut app, origin, &mut tick);
        assert_eq!(app.summary_at(1), Some(app::Summary::Auto), "round again");
    }

    /// Walk the `a` wizard: kind, recipe, then the name (replacing what the
    /// name prompt offers).
    fn add_column(app: &mut App, kind: char, recipe: &str, name: &str) {
        app.handle_key(key('a'));
        app.handle_key(key(kind));
        type_str(app, recipe);
        app.handle_key(code(KeyCode::Enter));
        let offered = app.input.chars().count();
        for _ in 0..offered {
            app.handle_key(code(KeyCode::Backspace));
        }
        type_str(app, name);
        app.handle_key(code(KeyCode::Enter));
    }

    /// A column's values in view order.
    fn values(app: &App, col: usize) -> Vec<String> {
        app.data
            .cells(col, &app.view_rows(usize::MAX))
            .unwrap()
            .into_iter()
            .map(|v| v.unwrap_or_default())
            .collect()
    }

    #[test]
    fn a_column_can_be_pulled_out_of_another_with_a_pattern() {
        let tsv = "External ID\treads\n3-SSH0421\t4300\n7-SSH0422\t2500\nodd\t10\n";
        let mut app = App::new(Dataset::load(&write_text_fixture("tsv", tsv)).unwrap());
        // The source is the column under the cursor, so only the pattern is typed.
        add_column(&mut app, 'e', "^[0-9]-SSH(.*)", "sample_name");

        assert_eq!(app.data.ncols, 3);
        assert_eq!(app.data.column_names[2], "sample_name");
        assert_eq!(values(&app, 2), vec!["0421", "0422", ""]);
        assert!(
            app.data.is_null(2, 2),
            "a row the pattern does not match has no value, rather than an empty one"
        );
        // The padding is kept: `0421` is a name, not the number 421.
        assert_eq!(app.data.column_types[2], "Utf8");
        // It lands on screen at the end, selected.
        assert_eq!(app.visible_cols(), &[0, 1, 2]);
        assert_eq!(app.selected_pos, 2);

        // With no capture group, the whole match is kept. Back to the source
        // column first: an extraction reads whatever the cursor is on, and it
        // was left on the column just added.
        app.selected_pos = 0;
        add_column(&mut app, 'e', "SSH[0-9]+", "whole");
        assert_eq!(values(&app, 3), vec!["SSH0421", "SSH0422", ""]);
    }

    #[test]
    fn a_column_can_be_worked_out_by_formula() {
        let tsv = "sample\treads\ttotal\nS1\t4300\t10000\nS2\t2500\t10000\n";
        let mut app = App::new(Dataset::load(&write_text_fixture("tsv", tsv)).unwrap());

        // Arithmetic over two columns, typed as a number.
        add_column(&mut app, 'f', "{reads} / {total} * 100", "pct");
        assert_eq!(values(&app, 3), vec!["43", "25"]);
        assert!(app.data.is_numeric(3), "so it sorts and totals as a number");

        // Text joining, and a reference to a column that was itself computed.
        add_column(&mut app, 'f', "{sample} + \".sat\"", "sat");
        assert_eq!(values(&app, 4), vec!["S1.sat", "S2.sat"]);
        add_column(&mut app, 'f', "{sat} + \"/\" + {pct}", "both");
        assert_eq!(values(&app, 5), vec!["S1.sat/43", "S2.sat/25"]);

        // A literal alone is a constant column, which is `csvtk mutate3`.
        add_column(&mut app, 'f', "\"2026-08-20\"", "analysis_date");
        assert_eq!(values(&app, 6), vec!["2026-08-20", "2026-08-20"]);

        // Precedence and parentheses.
        add_column(&mut app, 'f', "({reads} + {total}) / 2", "mid");
        assert_eq!(values(&app, 7), vec!["7150", "6250"]);
    }

    /// Type a formula and commit it, expecting it to be refused.
    fn bad_formula(app: &mut App, recipe: &str) -> app::Notice {
        app.handle_key(key('a'));
        app.handle_key(key('f'));
        type_str(app, recipe);
        app.handle_key(code(KeyCode::Enter));
        let problem = app
            .notice
            .clone()
            .unwrap_or_else(|| panic!("{recipe} should have been refused"));
        // The prompt is still up, with the text still in it, so it can be fixed
        // rather than typed again.
        assert!(matches!(
            app.mode,
            app::Mode::Input(app::InputKind::Recipe)
        ));
        assert_eq!(app.input, recipe);
        app.handle_key(code(KeyCode::Esc));
        problem
    }

    #[test]
    fn a_formula_that_will_not_do_is_shown_back_with_the_spot_marked() {
        let tsv = "a\tb\n1\t2\n";
        let mut app = App::new(Dataset::load(&write_text_fixture("tsv", tsv)).unwrap());

        for (recipe, expected, caret) in [
            ("{nope} + 1", "there is no column called nope", Some(0)),
            ("{a} +", "stops here", Some(5)),
            ("{a} @ 2", "not something a formula can use", Some(4)),
            ("({a} + 1", "never closes", Some(0)),
            ("{a} 2", "left over at the end", Some(4)),
            ("* {a}", "needs a value before it", Some(0)),
            ("nope({a})", "there is no function `nope`", Some(0)),
            ("round()", "takes 1 or 2 arguments", Some(0)),
        ] {
            let problem = bad_formula(&mut app, recipe);
            assert!(
                problem.message.contains(expected),
                "{recipe}: got {:?}",
                problem.message
            );
            assert_eq!(problem.at, caret, "caret for {recipe}");
            assert_eq!(app.data.ncols, 2, "nothing added for {recipe}");
        }

        // An empty prompt is not a mistake to complain about, it is a change of
        // mind — as it is at every other prompt.
        app.handle_key(key('a'));
        app.handle_key(key('f'));
        app.handle_key(code(KeyCode::Enter));
        assert!(app.notice.is_none());
        assert!(app.new_column.is_none());
        assert_eq!(app.status_msg.as_deref(), Some("no new column"));

        // The complaint is drawn over the table, formula and caret and all.
        app.handle_key(key('a'));
        app.handle_key(key('f'));
        type_str(&mut app, "{a} ** ");
        app.handle_key(code(KeyCode::Enter));
        let text = buffer_text(&mut app, 80, 16);
        assert!(text.contains("{a} **"), "the formula is shown: {text}");
        assert!(text.contains('↑'), "with the spot marked: {text}");
        assert!(text.contains("stops here"), "and what is wrong: {text}");
        // Editing it clears the complaint, rather than leaving a stale caret.
        app.handle_key(code(KeyCode::Backspace));
        assert!(app.notice.is_none());
        app.handle_key(code(KeyCode::Esc));

        // A name already taken is refused at the naming step.
        add_column(&mut app, 'f', "1", "a");
        assert!(app.status_msg.as_deref().unwrap_or_default().contains("already"));
        assert_eq!(app.data.ncols, 2);
    }

    #[test]
    fn a_formula_can_slice_text_and_convert_it() {
        // An accented name, to be sure slicing counts characters not bytes.
        let tsv = "col\tPRÉLÈVEMENT\n3-SSH0421\tabcdéfg\n7-SSH0422\thijklmn\n";
        let mut app = App::new(Dataset::load(&write_text_fixture("tsv", tsv)).unwrap());

        // Python's shapes, meaning what they mean there.
        add_column(&mut app, 'f', "{col}[:-2]", "most");
        assert_eq!(values(&app, 2), vec!["3-SSH04", "7-SSH04"]);
        add_column(&mut app, 'f', "{col}[2:]", "tail");
        assert_eq!(values(&app, 3), vec!["SSH0421", "SSH0422"]);
        add_column(&mut app, 'f', "{col}[-3:-1]", "middle");
        assert_eq!(values(&app, 4), vec!["42", "42"]);
        add_column(&mut app, 'f', "{col}[:]", "all");
        assert_eq!(values(&app, 5), vec!["3-SSH0421", "7-SSH0422"]);
        // One character rather than a range.
        add_column(&mut app, 'f', "{col}[-1]", "last");
        assert_eq!(values(&app, 6), vec!["1", "2"]);
        // Characters, not bytes: this file has accents in it.
        add_column(&mut app, 'f', "{PRÉLÈVEMENT}[3:6]", "accents");
        assert_eq!(values(&app, 7), vec!["déf", "klm"]);
        // A range beyond the end is clamped; a single index beyond it is a gap,
        // the same asymmetry Python has.
        add_column(&mut app, 'f', "{col}[5:99]", "clamped");
        assert_eq!(values(&app, 8), vec!["0421", "0422"]);
        add_column(&mut app, 'f', "{col}[99]", "nothing_there");
        assert!(app.data.is_null(9, 0) && app.data.is_null(9, 1));
        // Slices chain.
        add_column(&mut app, 'f', "{col}[2:][0]", "first_of_tail");
        assert_eq!(values(&app, 10), vec!["S", "S"]);

        // A slice is text, so `float` is what makes it countable — which is why
        // it had to come with slicing rather than after it.
        add_column(&mut app, 'f', "float({col}[6:]) ** 2", "squared");
        assert_eq!(values(&app, 11), vec!["177241", "178084"]);
        assert!(app.data.is_numeric(11));
        add_column(&mut app, 'f', "int(float({col}[6:]) / 2)", "halved");
        assert_eq!(values(&app, 12), vec!["210", "211"], "int cuts towards zero");
        add_column(&mut app, 'f', "str(2 ** 10) + \"!\"", "as_text");
        assert_eq!(values(&app, 13), vec!["1024!", "1024!"]);
        // Text that is not a number converts to a gap, not a zero.
        add_column(&mut app, 'f', "float({PRÉLÈVEMENT})", "not_a_number");
        assert!(app.data.is_null(14, 0));
        // `int` is forgiving where Python refuses, reading what is there.
        add_column(&mut app, 'f', "int(\"3.7\")", "three");
        assert_eq!(values(&app, 15), vec!["3", "3"]);
    }

    #[test]
    fn a_slice_that_will_not_do_says_where() {
        let tsv = "col\n3-SSH0421\n";
        let mut app = App::new(Dataset::load(&write_text_fixture("tsv", tsv)).unwrap());
        for (recipe, expected, caret) in [
            ("{col}[", "stops here", Some(6)),
            ("{col}[]", "says nothing", Some(5)),
            ("{col}[1:2", "never closes", Some(5)),
            ("[1]", "nothing to take from", Some(0)),
            ("{col}]", "left over at the end", Some(5)),
            ("{col}[1:2:3]", "never closes", Some(5)),
        ] {
            let problem = bad_formula(&mut app, recipe);
            assert!(
                problem.message.contains(expected),
                "{recipe}: got {:?}",
                problem.message
            );
            assert_eq!(problem.at, caret, "caret for {recipe}");
        }
    }

    #[test]
    fn a_formula_can_do_powers_and_functions() {
        let tsv = "n\tm\n2\t9\n3\t16\n";
        let mut app = App::new(Dataset::load(&write_text_fixture("tsv", tsv)).unwrap());

        add_column(&mut app, 'f', "{n} ** 2", "squared");
        assert_eq!(values(&app, 2), vec!["4", "9"]);
        // `^` says the same thing, for spreadsheet fingers.
        add_column(&mut app, 'f', "{n} ^ 3", "cubed");
        assert_eq!(values(&app, 3), vec!["8", "27"]);
        // Powers bind tighter than `*` and go to the right.
        add_column(&mut app, 'f', "2 * {n} ** 2", "twice_sq");
        assert_eq!(values(&app, 4), vec!["8", "18"]);
        add_column(&mut app, 'f', "2 ** 3 ** 2", "right_assoc");
        assert_eq!(values(&app, 5), vec!["512", "512"]);
        // …and unary minus applies after the power, as on paper.
        add_column(&mut app, 'f', "-{n} ** 2", "neg_sq");
        assert_eq!(values(&app, 6), vec!["-4", "-9"]);

        add_column(&mut app, 'f', "sqrt({m})", "root");
        assert_eq!(values(&app, 7), vec!["3", "4"]);
        add_column(&mut app, 'f', "round(log({m}) * 100)", "log100");
        assert_eq!(values(&app, 8), vec!["95", "120"]);
        add_column(&mut app, 'f', "round(ln({m}), 2)", "ln2dp");
        assert_eq!(values(&app, 9), vec!["2.2", "2.77"]);
        add_column(&mut app, 'f', "max({n}, {m})", "bigger");
        assert_eq!(values(&app, 10), vec!["9", "16"]);

        // Anything that cannot be worked out leaves a gap, not an error.
        add_column(&mut app, 'f', "sqrt(0 - {n})", "impossible");
        assert_eq!(values(&app, 11), vec!["", ""]);
        assert!(app.data.is_null(11, 0));
    }

    #[test]
    fn a_computed_column_behaves_like_any_other() {
        let tsv = "sample\treads\nS2\t2500\nS1\t4300\n";
        let mut app = App::new(Dataset::load(&write_text_fixture("tsv", tsv)).unwrap());
        add_column(&mut app, 'f', "{reads} / 100", "hundreds");

        // Sorting by it orders numerically, not as text.
        app.handle_key(key('s'));
        assert_eq!(values(&app, 2), vec!["25", "43"]);
        // The summary line counts it.
        let (origin, mut tick) = (Instant::now(), 0);
        tap_summary(&mut app, origin, &mut tick);
        assert!(summary_line(&mut app, 60, 9).contains("68"), "25 + 43");
        // Filtering sees it.
        app.handle_key(key('&'));
        type_str(&mut app, "^43$");
        app.handle_key(code(KeyCode::Enter));
        assert_eq!(app.row_count(), 1);
        app.handle_key(code(KeyCode::Esc));
        // And it can be hidden and brought back like any column.
        app.selected_pos = 2;
        app.handle_key(key('x'));
        assert_eq!(app.visible_cols(), &[0, 1]);
        app.handle_key(key('u'));
        assert_eq!(app.visible_cols(), &[0, 1, 2]);
    }

    #[test]
    fn adding_a_column_and_renaming_one_can_be_undone() {
        let tsv = "sample\treads\nS1\t4300\n";
        let mut app = App::new(Dataset::load(&write_text_fixture("tsv", tsv)).unwrap());
        add_column(&mut app, 'f', "{reads} * 2", "double");
        assert_eq!(app.data.ncols, 3);

        app.handle_key(key('z'));
        assert_eq!(app.status_msg.as_deref(), Some("undid new column"));
        assert_eq!(app.data.ncols, 2, "the column is gone");
        assert_eq!(app.visible_cols(), &[0, 1]);
        app.handle_key(key('Z'));
        assert_eq!(app.data.ncols, 3, "and comes back");
        assert_eq!(values(&app, 2), vec!["8600"]);

        // Renaming, and putting the name back.
        app.selected_pos = 0;
        app.handle_key(key('R'));
        for _ in 0.."sample".len() {
            app.handle_key(code(KeyCode::Backspace));
        }
        type_str(&mut app, "id");
        app.handle_key(code(KeyCode::Enter));
        assert_eq!(app.data.column_names[0], "id");
        app.handle_key(key('z'));
        assert_eq!(app.status_msg.as_deref(), Some("undid rename"));
        assert_eq!(app.data.column_names[0], "sample");
    }

    #[test]
    fn a_pattern_remembers_computed_columns_and_names() {
        let store = store_path();
        let tsv = "External ID\treads\n3-SSH0421\t4300\n7-SSH0422\t2500\n";
        let file = write_text_fixture("tsv", tsv);
        let name = file.file_name().unwrap().to_string_lossy().into_owned();
        {
            let mut tabs = Tabs::open(
                std::slice::from_ref(&file),
                HeaderSpec::default(),
                Store::load_at(store.clone()),
            )
            .unwrap();
            add_column(tabs.app_mut(), 'e', "^[0-9]-SSH(.*)", "sample_name");
            add_column(tabs.app_mut(), 'f', "{sample_name} + \".sat\"", "sat");
            tabs.app_mut().selected_pos = 0;
            tabs.key(key('R'));
            for _ in 0.."External ID".len() {
                tabs.key(KeyEvent::from(KeyCode::Backspace));
            }
            type_str(tabs.app_mut(), "id");
            tabs.key(KeyEvent::from(KeyCode::Enter));
            save_pattern(&mut tabs, &name);
        }
        // Written by name, and readable.
        let text = std::fs::read_to_string(&store).unwrap();
        assert!(text.contains("\"sample_name\""), "{text}");
        assert!(text.contains("^[0-9]-SSH(.*)"), "{text}");

        let mut tabs =
            Tabs::open(std::slice::from_ref(&file), HeaderSpec::default(), Store::load_at(store))
                .unwrap();
        let app = tabs.app_mut();
        assert_eq!(app.data.column_names, vec!["id", "reads", "sample_name", "sat"]);
        assert_eq!(values(app, 3), vec!["0421.sat", "0422.sat"]);
    }

    #[test]
    fn a_scope_applies_a_column_command_to_a_block() {
        let csv = "name,a,b,c\nx,1.5,2.5,3.5\ny,2.5,3.5,4.5\n";
        let mut app = App::new(Dataset::load(&write_text_fixture("csv", csv)).unwrap());
        let (origin, mut tick) = (Instant::now(), 0);

        // `(` aims the next column command at this column and those right of it.
        app.selected_pos = 2; // b
        app.handle_key(key('('));
        assert_eq!(app.scope, Some(app::Scope::Rightward));
        assert_eq!(app.scoped_span(), (2, 3));
        app.handle_key(key('%'));
        assert!(app.num_styles.contains_key(&2) && app.num_styles.contains_key(&3));
        assert!(!app.num_styles.contains_key(&1), "the column to the left is not");
        assert!(
            app.status_msg.as_deref().unwrap_or_default().contains("2 columns"),
            "unexpected: {:?}",
            app.status_msg
        );

        // The scope lasts for a run of column commands …
        app.handle_key(key('>'));
        assert_eq!(app.num_styles[&2].decimals, app.num_styles[&3].decimals);
        // … and a scoped command evens the block out rather than nudging each
        // column from wherever it was.
        assert_eq!(app.num_styles[&3].decimals, Some(3));

        // … but anything that is not a column command drops it.
        app.handle_key(key('j'));
        assert!(app.scope.is_none());
        // A key that means nothing drops a pending aim rather than sitting on it.
        app.handle_key(key('('));
        app.handle_key(key('p'));
        assert!(app.scope.is_none(), "an unbound key drops the aim");
        assert!(app.resize.is_none(), "and starts nothing");

        // `)` covers this column and everything to its left.
        app.selected_pos = 1;
        app.handle_key(key(')'));
        assert_eq!(app.scoped_span(), (0, 1));

        // Summary cycling takes the block too, all to the same thing.
        app.handle_key(key('u')); // clear the styling first
        tap_summary(&mut app, origin, &mut tick); // line on
        app.selected_pos = 1;
        app.handle_key(key('('));
        tap_summary(&mut app, origin, &mut tick);
        assert_eq!(app.summary_at(1), Some(app::Summary::Total));
        assert_eq!(app.summary_at(3), Some(app::Summary::Total), "and the block");
        // Cycling again keeps the scope, without pressing `(` a second time.
        tap_summary(&mut app, origin, &mut tick);
        assert_eq!(app.summary_at(1), Some(app::Summary::Mean));
        assert_eq!(app.summary_at(3), Some(app::Summary::Mean));
    }

    #[test]
    fn a_scope_hides_a_block_and_resizes_one() {
        let csv = "a,b,c,d\n1,2,3,4\n";
        let mut app = App::new(Dataset::load(&write_text_fixture("csv", csv)).unwrap());

        // `( x` trims everything from here rightwards — the useful one after a
        // join has left too many columns.
        app.selected_pos = 2;
        app.handle_key(key('('));
        app.handle_key(key('x'));
        assert_eq!(app.visible_cols(), &[0, 1]);
        assert!(app.status_msg.as_deref().unwrap_or_default().contains("2 columns"));
        // One column always survives, however wide the scope.
        app.handle_key(key('u'));
        app.selected_pos = 0;
        app.handle_key(key('('));
        app.handle_key(key('x'));
        assert_eq!(app.visible_cols().len(), 1, "the last column stays");
        assert!(app.status_msg.as_deref().unwrap_or_default().contains("one kept"));
        app.handle_key(key('u'));

        // A resize spends the scope on the way in, since it is a whole
        // interaction rather than a repeatable keypress.
        app.selected_pos = 1;
        app.handle_key(key('('));
        app.handle_key(key('r'));
        assert_eq!(app.resize.as_ref().unwrap().count, 3, "b, c and d");
        assert!(app.scope.is_none(), "the resize took it");
        app.handle_key(code(KeyCode::Enter));

        // And a plain `r` is one column.
        app.handle_key(key('r'));
        assert_eq!(app.resize.as_ref().unwrap().count, 1);
    }

    #[test]
    fn holding_the_summary_key_puts_the_line_away() {
        use std::time::{Duration, Instant};
        let csv = "n\n1\n2\n";
        let mut app = App::new(Dataset::load(&write_text_fixture("csv", csv)).unwrap());
        let (t0, mut tick) = (Instant::now(), 0);
        tap_summary(&mut app, t0, &mut tick);
        assert!(app.summary.is_some());

        // Auto-repeat: presses far faster than anyone taps.
        for i in 0..8 {
            app.handle_key_at(key('='), t0 + Duration::from_millis(5_000 + i * 30));
        }
        assert!(app.summary.is_none(), "holding it removes the line");
        assert_eq!(app.status_msg.as_deref(), Some("summary off"));

        // Releasing and pressing again brings it back, rather than the run
        // continuing to toggle it.
        app.handle_key_at(key('='), t0 + Duration::from_millis(9_000));
        assert_eq!(app.summary, Some(app::Summary::Auto));
    }

    #[test]
    fn the_summary_counts_only_the_rows_on_display() {
        let csv = "grp,n\na,1\nb,10\na,100\n";
        let mut app = App::new(Dataset::load(&write_text_fixture("csv", csv)).unwrap());
        let (origin, mut tick) = (Instant::now(), 0);
        tap_summary(&mut app, origin, &mut tick);
        assert!(summary_line(&mut app, 40, 9).contains("111"));

        // Filtering changes what is counted …
        app.handle_key(key('&'));
        type_str(&mut app, "^a$");
        app.handle_key(code(KeyCode::Enter));
        assert_eq!(app.row_count(), 2);
        assert!(summary_line(&mut app, 40, 9).contains("101"), "filtered total");

        // … while sorting only changes the order, so the total stands.
        app.handle_key(code(KeyCode::Esc)); // clear the filter
        app.selected_pos = 1;
        app.handle_key(key('s'));
        assert!(summary_line(&mut app, 40, 9).contains("111"), "unchanged by a sort");

        // And undo restores the total along with the rows it counted.
        app.handle_key(key('&'));
        type_str(&mut app, "^a$");
        app.handle_key(code(KeyCode::Enter));
        assert!(summary_line(&mut app, 40, 9).contains("101"));
        app.handle_key(key('z'));
        assert!(summary_line(&mut app, 40, 9).contains("111"), "after undo");
    }

    #[test]
    fn a_pattern_remembers_the_summary() {
        let store = store_path();
        let file = write_text_fixture("csv", "n\n1\n2\n3\n");
        let name = file.file_name().unwrap().to_string_lossy().into_owned();
        {
            let mut tabs = Tabs::open(
                std::slice::from_ref(&file),
                HeaderSpec::default(),
                Store::load_at(store.clone()),
            )
            .unwrap();
            // auto → total → mean
            let (origin, mut tick) = (Instant::now(), 0);
            for _ in 0..3 {
                tap_summary(tabs.app_mut(), origin, &mut tick);
            }
            let col = tabs.app_mut().selected_col();
            assert_eq!(tabs.app_mut().summary_at(col), Some(app::Summary::Mean));
            save_pattern(&mut tabs, &name);
        }
        let mut tabs =
            Tabs::open(std::slice::from_ref(&file), HeaderSpec::default(), Store::load_at(store))
                .unwrap();
        let col = tabs.app_mut().selected_col();
        assert_eq!(
            tabs.app_mut().summary_at(col),
            Some(app::Summary::Mean),
            "the column's own mode came back, not just the line"
        );
        assert!(summary_line(tabs.app_mut(), 40, 9).contains('2'), "the mean of 1..3");
    }

    /// A session file of its own, and a folder to hold a project.
    fn project() -> (PathBuf, PathBuf) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let mut folder = std::env::temp_dir();
        folder.push(format!("lambris_test_project_{n}"));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        let mut sessions = std::env::temp_dir();
        sessions.push(format!("lambris_test_sessions_{n}.json"));
        let _ = std::fs::remove_file(&sessions);
        (folder, sessions)
    }

    #[test]
    fn a_session_reopens_the_tabs_that_were_here() {
        let (folder, store) = project();
        std::fs::write(folder.join("meta.csv"), "sample,depth,junk\nS1,10,x\nS2,20,y\n")
            .unwrap();
        std::fs::write(folder.join("dict.csv"), "sample,label\nS1,control\n").unwrap();
        let files = vec![folder.join("meta.csv"), folder.join("dict.csv")];

        {
            let mut tabs =
                Tabs::open(&files, HeaderSpec::default(), Store::default()).unwrap();
            tabs.folder = folder.clone();
            tabs.sessions = Sessions::load_at(store.clone());
            // Arrange the first tab, and leave the second one in front.
            tabs.app_mut().selected_pos = 2;
            tabs.key(key('x')); // hide junk
            tabs.key(key('s')); // and sort
            tabs.key(KeyEvent::from(KeyCode::Tab));
            assert!(tabs.step());
            tabs.key(key('W'));
            assert!(tabs.step());
            let msg = tabs.app_mut().status_msg.clone().unwrap_or_default();
            assert!(msg.contains("remembered 2 tab"), "unexpected: {msg}");
        }

        // Files are written relative to the folder, so a project can move.
        let text = std::fs::read_to_string(&store).unwrap();
        assert!(text.contains("\"meta.csv\""), "{text}");
        assert!(!text.contains(folder.join("meta.csv").to_str().unwrap()));

        // Reopened with no files named at all.
        let sessions = Sessions::load_at(store.clone());
        let mut tabs =
            Tabs::reopen(&folder, &sessions, HeaderSpec::default(), Store::default()).unwrap();
        assert_eq!(tabs.tabs.len(), 2);
        assert_eq!(tabs.current, 1, "the tab that was in front");
        tabs.current = 0;
        let app = tabs.app_mut();
        assert_eq!(app.data.label, "meta.csv");
        assert_eq!(app.visible_cols(), &[0, 1], "junk is still hidden");
        assert!(app.sort.is_some(), "and it is still sorted");
    }

    #[test]
    fn quitting_with_a_changed_session_asks_first() {
        let (folder, store) = project();
        std::fs::write(folder.join("a.csv"), "k,v\nx,1\ny,2\n").unwrap();
        let files = vec![folder.join("a.csv")];
        let open = |store: &PathBuf| {
            let mut tabs =
                Tabs::open(&files, HeaderSpec::default(), Store::default()).unwrap();
            tabs.folder = folder.clone();
            tabs.sessions = Sessions::load_at(store.clone());
            tabs
        };

        // With nothing saved for this folder there is nothing to lose, so an
        // ordinary run quits without a word.
        let mut tabs = open(&store);
        tabs.key(key('x'));
        tabs.key(key('q'));
        assert!(!tabs.step(), "no session here, so no question");

        // Once saved, quitting unchanged is still silent.
        let mut tabs = open(&store);
        tabs.key(key('W'));
        assert!(tabs.step());
        tabs.key(key('q'));
        assert!(!tabs.step(), "nothing has changed since");

        // Change something, and it asks.
        let mut tabs = open(&store);
        tabs.key(key('x'));
        tabs.key(key('q'));
        assert!(tabs.step(), "quitting is held up");
        assert!(tabs.app_mut().quit_question);
        let text = buffer_text(tabs.app_mut(), 80, 16);
        assert!(text.contains("changed since it was saved"), "{text}");
        assert!(text.contains("y save · n discard"), "and how to answer: {text}");

        // A key that is not an answer decides nothing.
        tabs.key(key('j'));
        assert!(tabs.app_mut().quit_question, "still asking");
        assert!(tabs.step());

        // Esc goes back to the table, with the change still there.
        tabs.key(KeyEvent::from(KeyCode::Esc));
        assert!(tabs.step(), "still running");
        assert!(!tabs.app_mut().quit_question);
        assert!(tabs.app_mut().notice.is_none());
        assert_eq!(tabs.app_mut().visible_cols(), &[1], "the change is kept");

        // `n` leaves without saving: what was stored is untouched.
        tabs.key(key('q'));
        assert!(tabs.step());
        tabs.key(key('n'));
        assert!(!tabs.step(), "quit");
        let saved = Sessions::load_at(store.clone());
        let session = saved.for_folder(&folder).expect("still there");
        assert_eq!(session.tabs[0].view.hidden, Vec::<String>::new(), "not saved");

        // `y` saves on the way out.
        let mut tabs = open(&store);
        tabs.key(key('x'));
        tabs.key(key('q'));
        assert!(tabs.step());
        tabs.key(key('y'));
        assert!(!tabs.step(), "quit");
        let saved = Sessions::load_at(store);
        let session = saved.for_folder(&folder).expect("still there");
        assert_eq!(session.tabs[0].view.hidden, vec!["k".to_string()], "saved");
    }

    #[test]
    fn looking_at_another_tab_is_not_a_change_worth_asking_about() {
        let (folder, store) = project();
        std::fs::write(folder.join("a.csv"), "k\n1\n").unwrap();
        std::fs::write(folder.join("b.csv"), "k\n2\n").unwrap();
        let files = vec![folder.join("a.csv"), folder.join("b.csv")];
        let mut tabs = Tabs::open(&files, HeaderSpec::default(), Store::default()).unwrap();
        tabs.folder = folder.clone();
        tabs.sessions = Sessions::load_at(store);
        tabs.key(key('W'));
        assert!(tabs.step());

        // Moving about, and moving the cursor, change nothing that is saved.
        tabs.key(KeyEvent::from(KeyCode::Tab));
        assert!(tabs.step());
        tabs.key(key('j'));
        tabs.key(key('q'));
        assert!(!tabs.step(), "quits without asking");
    }

    #[test]
    fn a_session_remakes_a_joined_tab() {
        let (folder, store) = project();
        std::fs::write(folder.join("a.csv"), "k,v\nx,1\ny,2\n").unwrap();
        std::fs::write(folder.join("b.csv"), "k,w\nx,9\n").unwrap();
        let files = vec![folder.join("a.csv"), folder.join("b.csv")];
        let mut tabs = Tabs::open(&files, HeaderSpec::default(), Store::default()).unwrap();
        tabs.folder = folder.clone();
        tabs.sessions = Sessions::load_at(store.clone());

        // A join makes a third tab that is worked out rather than read.
        run_wizard(&mut tabs, 0, 1, 0);
        assert_eq!(tabs.tabs.len(), 3);
        // And transpose the first.
        tabs.current = 0;
        tabs.key(key('t'));
        assert!(tabs.step());
        assert!(tabs.app_mut().is_transposed);

        tabs.key(key('W'));
        assert!(tabs.step());
        let msg = tabs.app_mut().status_msg.clone().unwrap_or_default();
        assert!(msg.contains("remembered 3 tab"), "all three: {msg}");
        assert!(!msg.contains("left out"), "including the join: {msg}");

        // A join holds no data of its own, so it is written down as the recipe
        // it is: the two tabs and the key columns.
        let text = std::fs::read_to_string(&store).unwrap();
        assert!(text.contains("\"join\""), "{text}");
        assert!(text.contains("\"left_key\": \"k\""), "{text}");

        let sessions = Sessions::load_at(store);
        let mut tabs =
            Tabs::reopen(&folder, &sessions, HeaderSpec::default(), Store::default()).unwrap();
        assert_eq!(tabs.tabs.len(), 3, "the join was made again");
        tabs.current = 0;
        assert!(tabs.app_mut().is_transposed, "transposed again");
        // …and `t` still steps back to the file underneath.
        tabs.key(key('t'));
        assert!(tabs.step());
        assert_eq!(tabs.app_mut().data.column_names, vec!["k", "v"]);

        // The joined tab holds what it held before.
        tabs.current = 2;
        let app = tabs.app_mut();
        assert_eq!(app.data.column_names, vec!["k", "v", "w"]);
        // A left join, so `y` is still there with nothing beside it.
        assert_eq!(app.row_count(), 2);
        assert_eq!(app.data.cell_display(2, 0).unwrap().as_deref(), Some("9"));
        assert!(app.data.is_null(2, 1), "y matched nothing");
        assert!(app.origin.is_some(), "and still knows where it came from");
    }

    #[test]
    fn what_a_join_remembers_follows_the_tabs_it_came_from() {
        let (folder, store) = project();
        std::fs::write(folder.join("spare.csv"), "z\n1\n").unwrap();
        std::fs::write(folder.join("a.csv"), "k,v\nx,1\n").unwrap();
        std::fs::write(folder.join("b.csv"), "k,w\nx,9\n").unwrap();
        let files = vec![
            folder.join("spare.csv"),
            folder.join("a.csv"),
            folder.join("b.csv"),
        ];
        let mut tabs = Tabs::open(&files, HeaderSpec::default(), Store::default()).unwrap();
        tabs.folder = folder.clone();
        tabs.sessions = Sessions::load_at(store.clone());

        tabs.current = 1; // join a × b
        run_wizard(&mut tabs, 0, 2, 0);
        let origin = tabs.app_mut().origin.clone().expect("where it came from");
        assert_eq!((origin.left_tab, origin.right_tab), (1, 2));

        // Closing an unrelated tab before them shifts both sides down with it.
        tabs.current = 0;
        tabs.key(ctrl('w'));
        assert!(tabs.step());
        tabs.current = tabs.tabs.len() - 1;
        let origin = tabs.app_mut().origin.clone().expect("still knows");
        assert_eq!((origin.left_tab, origin.right_tab), (0, 1), "moved down");

        // Closing a side it actually came from drops it: there would be nothing
        // to make it from again.
        tabs.current = 0;
        tabs.key(ctrl('w'));
        assert!(tabs.step());
        tabs.current = tabs.tabs.len() - 1;
        assert!(tabs.app_mut().origin.is_none());
        tabs.key(key('W'));
        assert!(tabs.step());
        let msg = tabs.app_mut().status_msg.clone().unwrap_or_default();
        assert!(msg.contains("1 left out"), "unexpected: {msg}");
    }

    #[test]
    fn a_session_says_when_it_has_nothing_to_reopen() {
        let (folder, store) = project();
        let sessions = Sessions::load_at(store.clone());
        // Nothing saved here at all.
        let err = match Tabs::reopen(&folder, &sessions, HeaderSpec::default(), Store::default())
        {
            Ok(_) => panic!("there is no session here to reopen"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("nothing saved for"), "{err}");
        assert!(err.contains("press W"), "and says what to do: {err}");

        // A session whose files have since gone.
        std::fs::write(folder.join("gone.csv"), "a\n1\n").unwrap();
        {
            let mut tabs = Tabs::open(
                std::slice::from_ref(&folder.join("gone.csv")),
                HeaderSpec::default(),
                Store::default(),
            )
            .unwrap();
            tabs.folder = folder.clone();
            tabs.sessions = Sessions::load_at(store.clone());
            tabs.key(key('W'));
            assert!(tabs.step());
        }
        std::fs::remove_file(folder.join("gone.csv")).unwrap();
        let sessions = Sessions::load_at(store);
        let err = match Tabs::reopen(&folder, &sessions, HeaderSpec::default(), Store::default())
        {
            Ok(_) => panic!("the file is gone, so there is nothing to show"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("could be opened"), "{err}");
    }

    #[test]
    fn a_pattern_brings_the_arrangement_back() {
        let store = store_path();
        let csv = "id,name,score,junk\n2,beta,20,x\n1,alpha,10,y\n";
        let file = write_text_fixture("csv", csv);
        let name = file.file_name().unwrap().to_string_lossy().into_owned();

        {
            let mut tabs = Tabs::open(
                std::slice::from_ref(&file),
                HeaderSpec::default(),
                Store::load_at(store.clone()),
            )
            .unwrap();
            // Tune it: drop a column, move one, set a width, sort, freeze, and
            // turn the gutter off.
            tabs.app_mut().selected_pos = 3;
            tabs.key(key('x'));
            tabs.app_mut().selected_pos = 2;
            tabs.key(key('['));
            tabs.key(key('r'));
            tabs.key(KeyEvent::from(KeyCode::Right));
            tabs.key(KeyEvent::from(KeyCode::Enter));
            tabs.app_mut().selected_pos = 0;
            tabs.key(key('s'));
            tabs.key(key('f'));
            tabs.key(key('#'));
            let arranged = tabs.app_mut().visible_cols().to_vec();
            assert_eq!(arranged, vec![0, 2, 1]);

            save_pattern(&mut tabs, &name);
            assert!(
                tabs.app_mut().status_msg.as_deref() == Some(&format!("remembered for {name}")),
                "unexpected: {:?}",
                tabs.app_mut().status_msg
            );
        }

        // It is written by column *name*, not by position.
        let text = std::fs::read_to_string(&store).unwrap();
        assert!(text.contains("\"score\""), "names not written: {text}");
        assert!(text.contains(&name));

        // Opening it again brings the arrangement back …
        let mut tabs =
            Tabs::open(std::slice::from_ref(&file), HeaderSpec::default(), Store::load_at(store.clone()))
                .unwrap();
        let app = tabs.app_mut();
        assert_eq!(app.visible_cols(), &[0, 2, 1], "order and hidden column");
        assert_eq!(app.col_width(2), Some(6));
        assert_eq!(app.sort.unwrap().col, 0);
        assert_eq!(app.frozen_cols, 1);
        assert!(!app.show_line_numbers);
        assert_eq!(
            app.data.cells(0, &app.view_rows(usize::MAX)).unwrap(),
            vec![Some("1".into()), Some("2".into())],
            "the sort came back too"
        );
        // … and it is the starting point, not a change to undo.
        tabs.key(key('z'));
        assert_eq!(tabs.app_mut().status_msg.as_deref(), Some("nothing to undo"));

        // …unless asked to ignore it, which is what --no-pattern does.
        let mut plain =
            Tabs::open(&[file], HeaderSpec::default(), Store::default()).unwrap();
        assert_eq!(plain.app_mut().visible_cols(), &[0, 1, 2, 3]);
    }

    #[test]
    fn a_pattern_follows_column_names_when_the_file_changes() {
        // This is why positions are not saved: the file is rewritten with its
        // columns in a different order, one gone and one new.
        let store = store_path();
        let file = write_text_fixture("csv", "a,b,c\n1,2,3\n");
        let name = file.file_name().unwrap().to_string_lossy().into_owned();
        {
            let mut tabs = Tabs::open(
                std::slice::from_ref(&file),
                HeaderSpec::default(),
                Store::load_at(store.clone()),
            )
            .unwrap();
            tabs.app_mut().selected_pos = 1;
            tabs.key(key('x')); // hide b
            tabs.app_mut().selected_pos = 1;
            tabs.key(key('[')); // c before a
            assert_eq!(tabs.app_mut().visible_cols(), &[2, 0]);
            save_pattern(&mut tabs, &name);
        }

        // `a` is now third, `c` first, `b` is still there and `d` is new.
        std::fs::write(&file, "c,b,a,d\n3,2,1,4\n").unwrap();
        let mut tabs =
            Tabs::open(&[file], HeaderSpec::default(), Store::load_at(store)).unwrap();
        let app = tabs.app_mut();
        let shown: Vec<&str> = app
            .visible_cols()
            .iter()
            .map(|&c| app.data.column_names[c].as_str())
            .collect();
        assert_eq!(
            shown,
            vec!["c", "a", "d"],
            "c then a as saved, b still hidden, and the new d kept visible"
        );
    }

    #[test]
    fn a_pattern_can_be_tied_to_a_glob() {
        let store = store_path();
        let first = write_text_fixture("tsv", "x\ty\n1\t2\n");
        {
            let mut tabs = Tabs::open(
                std::slice::from_ref(&first),
                HeaderSpec::default(),
                Store::load_at(store.clone()),
            )
            .unwrap();
            tabs.key(key('#')); // something visible to check for
            save_pattern(&mut tabs, "*.tsv");
        }
        // Any other .tsv picks it up.
        let other = write_text_fixture("tsv", "p\tq\n7\t8\n");
        let mut tabs =
            Tabs::open(&[other], HeaderSpec::default(), Store::load_at(store.clone()))
                .unwrap();
        assert!(!tabs.app_mut().show_line_numbers, "the glob matched");
        assert!(
            tabs.app_mut()
                .status_msg
                .as_deref()
                .unwrap_or_default()
                .contains("*.tsv"),
            "the pattern that matched should be named"
        );
        // A .csv does not.
        let csv = write_text_fixture("csv", "p,q\n7,8\n");
        let mut tabs =
            Tabs::open(&[csv], HeaderSpec::default(), Store::load_at(store)).unwrap();
        assert!(tabs.app_mut().show_line_numbers);
    }

    #[test]
    fn an_exact_binding_beats_a_glob_and_empty_forgets() {
        let store = store_path();
        let file = write_text_fixture("csv", "a,b\n1,2\n");
        let name = file.file_name().unwrap().to_string_lossy().into_owned();

        // A blanket glob hides nothing but turns the gutter off; the exact
        // binding for this file hides a column instead.
        {
            let mut tabs = Tabs::open(
                std::slice::from_ref(&file),
                HeaderSpec::default(),
                Store::load_at(store.clone()),
            )
            .unwrap();
            tabs.key(key('#'));
            save_pattern(&mut tabs, "*.csv");
            tabs.key(key('#')); // gutter back on
            tabs.key(key('x')); // hide a
            save_pattern(&mut tabs, &name);
        }
        let mut tabs = Tabs::open(
            std::slice::from_ref(&file),
            HeaderSpec::default(),
            Store::load_at(store.clone()),
        )
        .unwrap();
        assert_eq!(tabs.app_mut().visible_cols(), &[1], "the exact one won");
        assert!(tabs.app_mut().show_line_numbers, "…so the glob did not apply");

        // An empty binding forgets the pattern for this file, leaving the glob.
        save_pattern(&mut tabs, "");
        assert!(
            tabs.app_mut()
                .status_msg
                .as_deref()
                .unwrap_or_default()
                .starts_with("forgot the pattern"),
            "unexpected: {:?}",
            tabs.app_mut().status_msg
        );
        let mut tabs =
            Tabs::open(&[file], HeaderSpec::default(), Store::load_at(store)).unwrap();
        assert_eq!(tabs.app_mut().visible_cols(), &[0, 1], "back to the glob");
        assert!(!tabs.app_mut().show_line_numbers);
    }

    #[test]
    fn a_pattern_remembers_how_the_top_of_the_file_is_read() {
        // The header reading has to be settled before the file is decoded, so
        // this exercises the one part of a pattern that is applied at load.
        let store = store_path();
        let csv = "exported by hand,,\nnote,2026,-\nid,name,score\n1,alpha,3\n";
        let file = write_text_fixture("csv", csv);
        let name = file.file_name().unwrap().to_string_lossy().into_owned();
        {
            let mut tabs = Tabs::open(
                std::slice::from_ref(&file),
                HeaderSpec::default(),
                Store::load_at(store.clone()),
            )
            .unwrap();
            tabs.key(key('j'));
            tabs.key(key('H')); // the real header is row 3
            assert!(tabs.step(), "H re-reads the file from the loop");
            assert_eq!(tabs.app_mut().data.column_names, vec!["id", "name", "score"]);
            save_pattern(&mut tabs, &name);
        }
        let mut tabs =
            Tabs::open(&[file], HeaderSpec::default(), Store::load_at(store)).unwrap();
        let app = tabs.app_mut();
        assert_eq!(app.data.column_names, vec!["id", "name", "score"]);
        assert_eq!(app.data.header.skip(), 2);
        assert_eq!(app.row_count(), 1);
    }

    #[test]
    fn a_view_with_no_file_behind_it_cannot_be_remembered() {
        let store = store_path();
        let file = write_text_fixture("csv", "a,b\n1,2\n");
        let mut tabs =
            Tabs::open(&[file], HeaderSpec::default(), Store::load_at(store.clone())).unwrap();
        tabs.key(key('t')); // transpose: not the file's own layout
        assert!(tabs.step());
        tabs.key(key('w'));

        // Said at once, rather than after collecting a name that could never be
        // used — and said in the middle of the screen, where there is room for
        // the reason.
        let notice = tabs.app_mut().notice.clone().expect("a notice");
        assert!(notice.message.contains("belongs to a file"), "{}", notice.message);
        assert!(notice.hint.is_some(), "and says why");
        assert!(
            matches!(tabs.app_mut().mode, app::Mode::Normal),
            "no prompt should have opened"
        );
        let text = buffer_text(tabs.app_mut(), 80, 16);
        assert!(text.contains("belongs to a file"), "drawn over the table: {text}");
        assert!(!store.exists(), "nothing should have been written");

        // The next key puts it away, and does nothing else.
        tabs.key(key('j'));
        assert!(tabs.app_mut().notice.is_none());
        assert_eq!(tabs.app_mut().selected_row, 0, "the key was swallowed");
    }

    #[test]
    fn undo_and_redo_walk_back_and_forth() {
        let csv = "id,name\n3,c\n1,a\n2,b\n";
        let mut app = App::new(Dataset::load(&write_text_fixture("csv", csv)).unwrap());
        let order = |app: &App| -> Vec<String> {
            app.data
                .cells(0, &app.view_rows(usize::MAX))
                .unwrap()
                .into_iter()
                .flatten()
                .collect()
        };
        assert_eq!(order(&app), vec!["3", "1", "2"]);

        // Sort, filter, then hide a column.
        app.handle_key(key('s'));
        assert_eq!(order(&app), vec!["1", "2", "3"]);
        app.handle_key(key('&'));
        type_str(&mut app, "a");
        app.handle_key(code(KeyCode::Enter));
        assert_eq!(app.row_count(), 1);
        app.handle_key(key('x'));
        assert_eq!(app.visible_cols(), &[1]);

        // `z` walks back through each of them, saying what it undid.
        app.handle_key(key('z'));
        assert_eq!(app.status_msg.as_deref(), Some("undid hide column"));
        assert_eq!(app.visible_cols(), &[0, 1]);
        app.handle_key(key('z'));
        assert_eq!(app.status_msg.as_deref(), Some("undid filter"));
        assert_eq!(app.row_count(), 3);
        assert_eq!(order(&app), vec!["1", "2", "3"], "still sorted");
        app.handle_key(key('z'));
        assert_eq!(app.status_msg.as_deref(), Some("undid sort"));
        assert_eq!(order(&app), vec!["3", "1", "2"], "back to the file's order");
        assert!(app.sort.is_none());

        // And `Z` walks forward again.
        app.handle_key(key('Z'));
        assert_eq!(app.status_msg.as_deref(), Some("redid sort"));
        assert_eq!(order(&app), vec!["1", "2", "3"]);
        app.handle_key(key('Z'));
        assert_eq!(app.row_count(), 1);
        app.handle_key(key('Z'));
        assert_eq!(app.visible_cols(), &[1]);

        // Nothing left in either direction is said, not silently ignored.
        app.handle_key(key('Z'));
        assert_eq!(app.status_msg.as_deref(), Some("nothing to redo"));
        for _ in 0..4 {
            app.handle_key(key('z'));
        }
        assert_eq!(app.status_msg.as_deref(), Some("nothing to undo"));
    }

    #[test]
    fn undo_brings_back_what_u_threw_away() {
        // `u` restores every column at once, which can discard a lot of work.
        let csv = "a,b,c,d\n1,2,3,4\n";
        let mut app = App::new(Dataset::load(&write_text_fixture("csv", csv)).unwrap());
        app.selected_pos = 3;
        app.handle_key(key('x')); // hide d
        app.selected_pos = 0;
        app.handle_key(key(']')); // a and b swap
        app.handle_key(key('r')); // and a width
        app.handle_key(code(KeyCode::Left));
        app.handle_key(code(KeyCode::Enter));
        let arranged = app.visible_cols().to_vec();
        let width = app.col_width(app.selected_col());
        assert_eq!(arranged, vec![1, 0, 2]);
        assert!(width.is_some());

        app.handle_key(key('u'));
        assert_eq!(app.visible_cols(), &[0, 1, 2, 3]);
        assert_eq!(app.col_width(0), None);

        // One `z` puts the whole arrangement back.
        app.handle_key(key('z'));
        assert_eq!(app.status_msg.as_deref(), Some("undid restore columns"));
        assert_eq!(app.visible_cols(), arranged);
        assert_eq!(app.col_width(app.selected_col()), width);
    }

    #[test]
    fn undo_puts_back_a_sort_on_the_wrong_column() {
        // The other case that is hard to undo by hand: `s` cycles asc → desc →
        // off, so a mis-aimed sort takes three presses to clear — and a keyed
        // sort cannot be cleared by `s` at all without re-running the wizard.
        let csv = "id,name\n3,c\n1,a\n2,b\n";
        let mut app = App::new(Dataset::load(&write_text_fixture("csv", csv)).unwrap());
        app.selected_pos = 1;
        app.handle_key(key('S'));
        app.handle_key(code(KeyCode::Enter));
        app.handle_key(code(KeyCode::Enter));
        app.handle_key(key('a'));
        assert!(app.sort.unwrap().key.is_some());
        // `c` sorts last of a/b/c, and the cursor went with its record.
        assert_eq!(app.selected_row, 2);

        app.handle_key(key('z'));
        assert!(app.sort.is_none(), "the keyed sort is gone in one press");
        assert_eq!(app.selected_row, 0, "and the cursor is back where it was");
    }

    #[test]
    fn a_cancelled_change_leaves_no_step_behind() {
        let csv = "a,b\n1,2\n";
        let mut app = App::new(Dataset::load(&write_text_fixture("csv", csv)).unwrap());
        // A resize abandoned with Esc changes nothing, so there is nothing to
        // undo — pressing `z` must not consume an unrelated step.
        app.handle_key(key('f')); // one real change: freeze
        app.handle_key(key('r'));
        app.handle_key(code(KeyCode::Left));
        app.handle_key(code(KeyCode::Esc));
        app.handle_key(key('z'));
        assert_eq!(app.status_msg.as_deref(), Some("undid freeze"));
        assert_eq!(app.frozen_cols, 0);
    }

    #[test]
    fn doing_something_new_drops_the_forward_history() {
        let csv = "a\n1\n2\n";
        let mut app = App::new(Dataset::load(&write_text_fixture("csv", csv)).unwrap());
        app.handle_key(key('s'));
        app.handle_key(key('z'));
        assert!(app.sort.is_none());
        // A new change replaces what `Z` would have gone forward to.
        app.handle_key(key('f'));
        app.handle_key(key('Z'));
        assert_eq!(app.status_msg.as_deref(), Some("nothing to redo"));
        assert_eq!(app.frozen_cols, 1, "the new change stands");
    }

    #[test]
    fn the_history_has_a_bottom() {
        let csv = "a\n1\n";
        let mut app = App::new(Dataset::load(&write_text_fixture("csv", csv)).unwrap());
        // Comfortably more changes than the history keeps.
        for _ in 0..app::MAX_UNDO_STEPS + 8 {
            app.handle_key(key('f'));
        }
        let mut undone = 0;
        for _ in 0..app::MAX_UNDO_STEPS + 8 {
            app.handle_key(key('z'));
            if app.status_msg.as_deref() == Some("nothing to undo") {
                break;
            }
            undone += 1;
        }
        assert_eq!(undone, app::MAX_UNDO_STEPS, "the oldest steps are dropped");
    }

    #[test]
    fn column_width_can_be_set_kept_and_reverted() {
        let csv = "name,n\nalphabetical,1\nbetatestical,2\n";
        let mut app = App::new(Dataset::load(&write_text_fixture("csv", csv)).unwrap());
        // Sized to its contents to begin with.
        assert!(app.col_width(0).is_none());
        assert!(buffer_line(&mut app, 40, 8, 2).contains("alphabetical"));

        // `r` then ← narrows it, and the values clip to match.
        app.handle_key(key('r'));
        assert!(app.resize.is_some());
        for _ in 0..6 {
            app.handle_key(code(KeyCode::Left));
        }
        assert_eq!(app.col_width(0), Some(6));
        let row = buffer_line(&mut app, 40, 8, 2);
        assert!(row.contains("alpha…"), "not clipped to the set width: {row}");

        // Enter keeps it; the mode ends.
        app.handle_key(code(KeyCode::Enter));
        assert!(app.resize.is_none());
        assert_eq!(app.col_width(0), Some(6));

        // `0` inside a resize goes back to sizing by content.
        app.handle_key(key('r'));
        app.handle_key(key('0'));
        assert!(app.col_width(0).is_none());
        app.handle_key(code(KeyCode::Enter));

        // Esc puts back whatever the width was before that resize.
        app.handle_key(key('r'));
        for _ in 0..4 {
            app.handle_key(code(KeyCode::Left));
        }
        assert!(app.col_width(0).is_some());
        app.handle_key(code(KeyCode::Esc));
        assert_eq!(app.col_width(0), None, "reverted to what it was");
        assert_eq!(app.status_msg.as_deref(), Some("width unchanged"));

        // A width cannot be narrowed away entirely.
        app.handle_key(key('r'));
        for _ in 0..80 {
            app.handle_key(code(KeyCode::Left));
        }
        assert_eq!(app.col_width(0), Some(app::MIN_SET_WIDTH));
        // Both key pairs adjust, since neither is the obvious one for a width.
        app.handle_key(key('k'));
        app.handle_key(key('l'));
        assert_eq!(app.col_width(0), Some(app::MIN_SET_WIDTH + 2));
    }

    #[test]
    fn a_scoped_resize_evens_out_the_whole_block() {
        // Deliberately uneven: an 11-wide name beside a column needing 3.
        let csv = "aaaaaaaaaaa,b,c\n1,2,3\n";
        let mut app = App::new(Dataset::load(&write_text_fixture("csv", csv)).unwrap());
        app.handle_key(key('('));
        app.handle_key(key('r'));
        assert_eq!(app.resize.as_ref().unwrap().count, 3);

        // `R` evens them out straight away, so the narrow ones get *wider* —
        // that is the point, not a matching reduction.
        assert_eq!(app.col_width(0), Some(11));
        assert_eq!(app.col_width(1), Some(11), "the narrow column widened");
        assert_eq!(app.col_width(2), Some(11));

        // Adjusting keeps them equal rather than preserving old differences.
        app.handle_key(code(KeyCode::Left));
        app.handle_key(code(KeyCode::Left));
        assert_eq!(
            (app.col_width(0), app.col_width(1), app.col_width(2)),
            (Some(9), Some(9), Some(9))
        );
        app.handle_key(code(KeyCode::Enter));

        // A single `r` leaves the columns beside it alone.
        app.selected_pos = 1;
        app.handle_key(key('r'));
        app.handle_key(code(KeyCode::Right));
        app.handle_key(code(KeyCode::Enter));
        assert_eq!(app.col_width(1), Some(10));
        assert_eq!(app.col_width(0), Some(9), "its neighbours are untouched");

        // `u` forgets widths along with order and visibility.
        app.handle_key(key('u'));
        assert_eq!(app.col_width(0), None);
        assert_eq!(app.col_width(1), None);
    }

    #[test]
    fn percent_fits_each_column_to_its_values() {
        // The names are the long part here; the values are not.
        let csv = "a_long_name,v\n1,a_long_value_here\n2,b\n";
        let mut app = App::new(Dataset::load(&write_text_fixture("csv", csv)).unwrap());
        app.handle_key(key('('));
        app.handle_key(key('r'));
        app.handle_key(key('%'));

        // Each column now fits its own values, names aside — so unlike an
        // adjustment, `%` leaves them at different widths.
        assert_eq!(app.col_width(0), Some(1), "its values are one character");
        assert_eq!(app.col_width(1), Some(17), "a_long_value_here");

        // The name no longer fits, so the status bar carries it instead.
        let status = buffer_line(&mut app, 90, 8, 6);
        assert!(status.contains("a_long_name"), "clipped name missing: {status}");

        // Adjusting after `%` keeps the fit and moves them together.
        app.handle_key(code(KeyCode::Right));
        assert_eq!((app.col_width(0), app.col_width(1)), (Some(2), Some(18)));

        // `0` returns to sizing by name and content, and to evening out.
        app.handle_key(key('0'));
        assert_eq!(app.col_width(0), None);
        app.handle_key(code(KeyCode::Left));
        assert_eq!(
            app.col_width(0),
            app.col_width(1),
            "back to one width for both"
        );
    }

    #[test]
    fn clipped_cell_and_title_spill_into_the_status_bar() {
        let long_value = "this is a really rather long cell value";
        let csv = format!("a_very_long_column_name,n\n{long_value},1\n");
        let mut app = App::new(Dataset::load(&write_text_fixture("csv", &csv)).unwrap());

        // The layout is title, body, status, hints — so the status bar is the
        // second-to-last row and the hint line the last.
        const H: u16 = 8;
        const STATUS: u16 = H - 2;
        const HINTS: u16 = H - 1;

        // Nothing is clipped at full width, so nothing is reported.
        let status = buffer_line(&mut app, 100, H, STATUS);
        assert!(!status.contains('='), "nothing to report yet: {status}");

        // Narrow it until both the value and the title have to be cut.
        app.handle_key(key('r'));
        for _ in 0..30 {
            app.handle_key(code(KeyCode::Left));
        }
        app.handle_key(code(KeyCode::Enter));

        // Wide terminal: the value comes first, then the column's name.
        let status = buffer_line(&mut app, 110, H, STATUS);
        assert!(status.contains(long_value), "value missing: {status}");
        assert!(status.contains("a_very_long_column_name"), "title missing: {status}");
        assert!(
            status.find(long_value) < status.find("a_very_long_column_name"),
            "content should come first: {status}"
        );

        // Tight terminal: the content keeps the room and the title gives way.
        let status = buffer_line(&mut app, 62, H, STATUS);
        assert!(status.contains("this is a really"), "value dropped: {status}");
        assert!(
            !status.contains("a_very_long_column_name"),
            "title should have given way: {status}"
        );

        // Info mode already spells both out, so the status bar stops repeating.
        app.handle_key(key('i'));
        let status = buffer_line(&mut app, 110, H, STATUS);
        assert!(!status.contains(long_value), "repeated in info mode: {status}");
        let info = buffer_line(&mut app, 110, H, HINTS);
        assert!(info.contains(long_value), "info line should have it: {info}");
    }

    #[test]
    fn columns_can_be_hidden_moved_and_restored() {
        let csv = "id,name,score\n1,alpha,3\n2,beta,4\n";
        let mut app = App::new(Dataset::load(&write_text_fixture("csv", csv)).unwrap());
        assert_eq!(app.visible_cols(), &[0, 1, 2]);

        // `]` moves the selected column right, carrying the cursor with it.
        app.handle_key(key(']'));
        assert_eq!(app.visible_cols(), &[1, 0, 2]);
        assert_eq!(app.selected_pos, 1, "cursor follows the column");
        assert_eq!(app.selected_col(), 0, "…which is still `id`");
        let text = buffer_text(&mut app, 60, 10);
        let header = text.find("name").unwrap();
        assert!(header < text.find("id").unwrap(), "name now sits first: {text}");
        // Shift-arrows do the same thing where the terminal sends them.
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT));
        assert_eq!(app.visible_cols(), &[0, 1, 2]);

        // `[` and `]` stop at the ends rather than wrapping.
        app.handle_key(key('['));
        assert_eq!(app.visible_cols(), &[0, 1, 2]);

        // `x` hides the selected column; the cursor stays in range.
        app.selected_pos = 1;
        app.handle_key(key('x'));
        assert_eq!(app.visible_cols(), &[0, 2]);
        assert_eq!(app.hidden_count(), 1);
        let text = buffer_text(&mut app, 60, 10);
        assert!(!text.contains("alpha"), "hidden column still drawn: {text}");
        assert!(text.contains("1 hidden"), "no marker in the status bar: {text}");
        assert!(text.contains("col 2/2"), "count should be of shown columns: {text}");

        // `u` brings them all back, in the file's own order.
        app.handle_key(key('[')); // move something first
        app.handle_key(key('u'));
        assert_eq!(app.visible_cols(), &[0, 1, 2]);
        assert_eq!(app.hidden_count(), 0);
        assert!(buffer_text(&mut app, 60, 10).contains("alpha"));

        // Plain `u` restores columns, so Ctrl-u must still page up.
        app.selected_row = 20;
        app.handle_key(ctrl('u'));
        assert!(app.selected_row < 20, "Ctrl-u should still move rows");
        assert_eq!(app.visible_cols(), &[0, 1, 2], "…without touching columns");

        // The last column stays: an empty table is nothing to look at.
        app.handle_key(key('x'));
        app.handle_key(key('x'));
        assert_eq!(app.visible_cols().len(), 1);
        app.handle_key(key('x'));
        assert_eq!(app.visible_cols().len(), 1, "refused");
        assert_eq!(app.status_msg.as_deref(), Some("the last column stays"));
    }

    #[test]
    fn hidden_columns_are_left_out_of_search_filter_and_sort() {
        let csv = "id,secret\n1,findme\n2,other\n";
        let mut app = App::new(Dataset::load(&write_text_fixture("csv", csv)).unwrap());

        // Searching finds it while it is on display …
        app.handle_key(key('/'));
        type_str(&mut app, "findme");
        app.handle_key(code(KeyCode::Enter));
        assert_eq!((app.selected_row, app.selected_pos), (0, 1));

        // … and cannot land on it once hidden.
        app.selected_pos = 1;
        app.handle_key(key('x'));
        app.handle_key(key('n'));
        assert_eq!(app.status_msg.as_deref(), Some("no match"));
        assert_eq!(app.selected_pos, 0, "cursor stays on a shown column");

        // Filtering looks only at what is shown, too.
        app.handle_key(key('&'));
        type_str(&mut app, "findme");
        app.handle_key(code(KeyCode::Enter));
        assert_eq!(app.row_count(), 0, "the match is not on display");
    }

    #[test]
    fn transpose_and_join_use_the_columns_on_display() {
        // Hide a column, then transpose: it stays hidden, and the moved order
        // decides which column titles the records.
        let csv = "sample,depth,junk\nS1,10,x\nS2,20,y\n";
        let mut tabs =
            Tabs::open(&[write_text_fixture("csv", csv)], HeaderSpec::default(), Store::default()).unwrap();
        tabs.app_mut().selected_pos = 2;
        tabs.key(key('x')); // drop `junk`
        tabs.key(key('t'));
        assert!(tabs.step());
        let transposed = &tabs.app_mut().data;
        assert_eq!(transposed.column_names, vec!["field", "S1", "S2"]);
        assert_eq!(transposed.nrows, 1, "only `depth` became a row");
        tabs.key(key('t')); // back
        assert!(tabs.step());

        // Now join, with a column hidden on each side: the result carries only
        // the columns that were on display.
        let dict = write_text_fixture("csv", "sample,label,notes\nS1,control,n1\nS2,treated,n2\n");
        tabs.app_mut().handle_key(key('o'));
        type_str(tabs.app_mut(), dict.to_str().unwrap());
        tabs.app_mut().handle_key(code(KeyCode::Enter));
        assert!(tabs.step());
        tabs.app_mut().selected_pos = 2;
        tabs.key(key('x')); // drop `notes`
        assert_eq!(tabs.app_mut().visible_cols(), &[0, 1]);

        tabs.current = 0; // join the first tab onto the second
        run_wizard(&mut tabs, 0, 1, 0);
        let joined = &tabs.app_mut().data;
        assert_eq!(
            joined.column_names,
            vec!["sample", "depth", "label"],
            "junk and notes were both hidden"
        );
        assert_eq!(joined.nrows, 2);
    }

    #[test]
    fn join_wizard_combines_two_tabs() {
        let (meta, dict) = join_fixtures();
        let mut tabs = Tabs::open(&[meta, dict], HeaderSpec::default(), Store::default()).unwrap();

        // The banner leads the way, and only while the wizard is running.
        assert!(tabs.join_banner().is_none());
        tabs.key(key('J'));
        assert!(tabs.step());
        let banner = tabs.join_banner().unwrap();
        assert!(banner.contains("first key column"), "unexpected: {banner}");
        let text = buffer_text_banner(tabs.app_mut(), &banner, 100, 20);
        assert!(text.contains("first key column"), "banner not drawn: {text}");

        // First pick, then the banner names it and asks for the second.
        tabs.key(KeyEvent::from(KeyCode::Enter));
        assert!(tabs.step());
        let banner = tabs.join_banner().unwrap();
        assert!(banner.contains(".sample") && banner.contains("second"), "{banner}");

        tabs.key(KeyEvent::from(KeyCode::Tab));
        assert!(tabs.step());
        tabs.key(KeyEvent::from(KeyCode::Enter));
        assert!(tabs.step());

        // The result is a new tab, current, with both sides' columns.
        assert!(tabs.join_banner().is_none(), "the wizard is done");
        assert_eq!(tabs.tabs.len(), 3);
        assert_eq!(tabs.current, 2);
        let joined = &tabs.app_mut().data;
        // The right key column is dropped: it would repeat the left one.
        assert_eq!(joined.column_names, vec!["sample", "depth", "label"]);
        assert!(joined.label.contains('⋈'), "label: {}", joined.label);
        assert_eq!(joined.nrows, 3, "every left row is kept");
        assert_eq!(joined.cell_display(2, 0).unwrap().as_deref(), Some("control"));
        assert_eq!(joined.cell_display(2, 1).unwrap().as_deref(), Some("treated"));
        // S3 matched nothing, so the right side reads as null (shown as NA).
        assert!(joined.is_null(2, 2), "unmatched row should be null");
        // Types survive the join, so the joined column still sorts numerically.
        assert_eq!(joined.column_types[1], "Int64");
        assert!(joined.is_numeric(1));
        let msg = tabs.app_mut().status_msg.clone().unwrap_or_default();
        assert!(msg.contains("2 matched, 1 unmatched"), "unexpected: {msg}");
    }

    #[test]
    fn join_multiplies_rows_on_duplicate_keys_and_renames_clashes() {
        // `note` appears on both sides, and S1 appears twice on the right.
        let left = write_text_fixture("csv", "sample,note\nS1,a\nS2,b\n");
        let right = write_text_fixture(
            "csv",
            "sample,note\nS1,first\nS1,second\nS2,only\n",
        );
        let mut tabs = Tabs::open(&[left, right], HeaderSpec::default(), Store::default()).unwrap();
        run_wizard(&mut tabs, 0, 1, 0);

        let joined = &tabs.app_mut().data;
        assert_eq!(joined.column_names, vec!["sample", "note", "note_2"]);
        assert_eq!(joined.nrows, 3, "S1 matches twice, S2 once");
        assert_eq!(joined.cell_display(2, 0).unwrap().as_deref(), Some("first"));
        assert_eq!(joined.cell_display(2, 1).unwrap().as_deref(), Some("second"));
        assert_eq!(joined.cell_display(1, 0).unwrap().as_deref(), Some("a"));
    }

    #[test]
    fn join_uses_what_each_tab_is_showing() {
        let (meta, dict) = join_fixtures();
        let mut tabs = Tabs::open(&[meta, dict], HeaderSpec::default(), Store::default()).unwrap();

        // Filter the left tab down to one row; the join sees only that row.
        tabs.key(key('&'));
        type_str(tabs.app_mut(), "S2");
        tabs.key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(tabs.app_mut().row_count(), 1);

        run_wizard(&mut tabs, 0, 1, 0);
        let joined = &tabs.app_mut().data;
        assert_eq!(joined.nrows, 1, "the filter carried into the join");
        assert_eq!(joined.cell_display(0, 0).unwrap().as_deref(), Some("S2"));
        assert_eq!(joined.cell_display(2, 0).unwrap().as_deref(), Some("treated"));
    }

    #[test]
    fn join_works_on_a_transposed_tab() {
        // Transposed, the dict's `sample` values become column headers and its
        // fields become rows — so joining it means joining what is on screen.
        let (meta, dict) = join_fixtures();
        let mut tabs = Tabs::open(&[dict, meta], HeaderSpec::default(), Store::default()).unwrap();
        tabs.key(key('t'));
        assert!(tabs.step());
        assert!(tabs.app_mut().is_transposed);
        assert_eq!(tabs.app_mut().data.column_names, vec!["field", "S1", "S2"]);

        // Join the transposed view's `field` column against meta's `depth`.
        // Nothing matches — the point is that it joins the table as displayed.
        run_wizard(&mut tabs, 0, 1, 1);
        let joined = &tabs.app_mut().data;
        assert_eq!(joined.column_names, vec!["field", "S1", "S2", "sample"]);
        assert_eq!(joined.nrows, 1, "one row: the transposed view's only row");
        assert_eq!(tabs.tabs.len(), 3);
        // The transposed tab is untouched and still transposed.
        tabs.current = 0;
        assert!(tabs.app_mut().is_transposed);
    }

    #[test]
    fn join_wizard_cancels_and_survives_a_closed_tab() {
        let (meta, dict) = join_fixtures();
        let mut tabs = Tabs::open(&[meta, dict], HeaderSpec::default(), Store::default()).unwrap();

        // Esc backs out of the wizard rather than clearing search or quitting.
        tabs.key(key('J'));
        assert!(tabs.step());
        tabs.key(KeyEvent::from(KeyCode::Esc));
        assert!(tabs.step(), "Esc must not quit while the wizard is up");
        assert!(tabs.join_banner().is_none());
        assert_eq!(tabs.tabs.len(), 2, "nothing joined");

        // Closing the very tab a pick is on cancels the wizard rather than
        // letting the pick slide onto whatever table takes that index.
        tabs.key(key('J'));
        assert!(tabs.step());
        tabs.key(KeyEvent::from(KeyCode::Enter));
        assert!(tabs.step());
        tabs.current = 0;
        tabs.key(ctrl('w'));
        assert!(tabs.step());
        assert!(tabs.join_banner().is_none(), "the wizard should be off");
        let msg = tabs.app_mut().status_msg.clone().unwrap_or_default();
        assert!(msg.contains("that tab was closed"), "unexpected: {msg}");
        assert_eq!(tabs.tabs.len(), 1);
    }

    #[test]
    fn join_pick_follows_its_tab_when_another_closes() {
        // Pick on the third tab, close the first: the pick must still mean the
        // same table, even though every index after it shifted down.
        let (meta, dict) = join_fixtures();
        let spare = write_text_fixture("csv", "z\n1\n");
        let mut tabs =
            Tabs::open(&[spare, meta, dict], HeaderSpec::default(), Store::default()).unwrap();
        tabs.current = 2;
        tabs.key(key('J'));
        assert!(tabs.step());
        tabs.key(KeyEvent::from(KeyCode::Enter)); // pick dict.sample on tab 2
        assert!(tabs.step());

        tabs.current = 0;
        tabs.key(ctrl('w'));
        assert!(tabs.step());
        assert_eq!(tabs.tabs.len(), 2);
        let banner = tabs.join_banner().expect("wizard still running");
        assert!(banner.contains(".sample"), "pick lost: {banner}");

        // Finishing it joins dict with meta, not with the wrong table.
        tabs.current = 0; // meta, after the shift
        tabs.app_mut().selected_pos = 0;
        tabs.key(KeyEvent::from(KeyCode::Enter));
        assert!(tabs.step());
        let joined = &tabs.app_mut().data;
        assert_eq!(joined.column_names, vec!["sample", "label", "depth"]);
        assert_eq!(joined.nrows, 2, "dict's two rows, both matched");
    }

    #[test]
    fn join_matches_across_types_and_ignores_blank_keys() {
        // The same key as a number on one side and text on the other, plus a
        // blank key that must not pair up with the other blank.
        let left = write_text_fixture("csv", "id,x\n1,a\n2,b\n,c\n");
        let right = write_text_fixture("tsv", "id\ty\n1\tone\n\tblank\n");
        let mut tabs = Tabs::open(&[left, right], HeaderSpec::default(), Store::default()).unwrap();
        assert_eq!(tabs.app_mut().data.column_types[0], "Int64");
        run_wizard(&mut tabs, 0, 1, 0);

        let joined = &tabs.app_mut().data;
        assert_eq!(joined.nrows, 3);
        assert_eq!(joined.cell_display(2, 0).unwrap().as_deref(), Some("one"));
        assert!(joined.is_null(2, 1), "id 2 has no match");
        assert!(joined.is_null(2, 2), "a blank key matches nothing");
    }

    #[test]
    fn question_mark_shows_every_key_and_scrolls() {
        let mut app = App::new(Dataset::load(&fixture()).unwrap());
        // The bottom line only teases; `?` is the way in.
        let text = buffer_text(&mut app, 100, 24);
        assert!(text.contains("? all keys"), "missing pointer to help: {text}");
        assert!(!text.contains("this page"), "help shown unasked: {text}");

        app.handle_key(key('?'));
        assert!(app.show_help);
        let text = buffer_text(&mut app, 100, 44);
        // The page opens on the first sections; the rest are a scroll away.
        for expected in ["Moving", "Finding", "Sorting", "Columns", "keys"] {
            assert!(text.contains(expected), "help missing {expected}: {text}");
        }
        assert!(text.contains("aim the next column command"));
        assert!(!text.contains("item_0000"), "table should be covered: {text}");

        // Keys drive the page, not the table behind it.
        app.handle_key(key('j'));
        assert_eq!(app.help_offset, 1);
        assert_eq!(app.selected_row, 0, "the cursor must not move behind the page");
        app.handle_key(key('k'));
        assert_eq!(app.help_offset, 0);
        // Scrolling past the end is clamped by the renderer.
        for _ in 0..200 {
            app.handle_key(key('j'));
        }
        let text = buffer_text(&mut app, 100, 24);
        assert!(text.contains("q / Ctrl-c"), "last section unreachable: {text}");
        assert!(text.contains("View"), "…and the section it belongs to: {text}");

        // And it closes without quitting.
        app.handle_key(key('?'));
        assert!(!app.show_help && !app.should_quit);
        assert_eq!(app.help_offset, 0);
        app.handle_key(key('?'));
        app.handle_key(KeyEvent::from(KeyCode::Esc));
        assert!(!app.show_help && !app.should_quit, "Esc closes, it does not quit");
    }

    #[test]
    fn promoting_a_row_to_header_drops_what_is_above() {
        // Two rows of title junk above the real header — the ill-conceived case.
        let csv = "exported by hand,,\nnote,2026,-\nid,name,score\n1,alpha,3\n2,beta,4\n";
        let mut tabs = Tabs::open(&[write_text_fixture("csv", csv)], HeaderSpec::default(), Store::default())
            .unwrap();
        assert_eq!(tabs.app_mut().data.column_names[0], "exported by hand");

        // Move to the row holding the real header and press H.
        tabs.app_mut().handle_key(key('j'));
        let selected = tabs.app_mut().selected_orig();
        let under_cursor = tabs.app_mut().data.cell_display(0, selected).unwrap();
        assert_eq!(under_cursor.as_deref(), Some("id"), "cursor is on the real header");
        tabs.app_mut().handle_key(key('H'));
        assert!(tabs.step());
        assert_eq!(tabs.app_mut().data.column_names, vec!["id", "name", "score"]);
        assert_eq!(tabs.app_mut().row_count(), 2, "only the real data rows remain");
        assert_eq!(tabs.app_mut().data.header.skip(), 2);
        assert_eq!(tabs.app_mut().data.header.header_line(), 3);
        // The score column is numeric now that the junk rows are gone.
        assert!(tabs.app_mut().data.is_numeric(2));
        assert!(buffer_text(tabs.app_mut(), 100, 20).contains("header@3"));

        // Pressing H again cancels it, back to the file as it comes.
        tabs.app_mut().handle_key(key('H'));
        assert!(tabs.step());
        assert_eq!(tabs.app_mut().data.header, HeaderSpec::Auto);
        assert_eq!(tabs.app_mut().data.column_names[0], "exported by hand");
        assert_eq!(tabs.tabs.len(), 1, "it stays one tab throughout");
    }

    #[test]
    fn promoting_counts_from_the_row_under_the_cursor() {
        // Header at the top, so the first data row is raw row 2.
        let csv = "id,name\nalpha,1\nbeta,2\n";
        let mut tabs = Tabs::open(&[write_text_fixture("csv", csv)], HeaderSpec::default(), Store::default())
            .unwrap();
        tabs.app_mut().handle_key(key('H'));
        assert!(tabs.step());
        assert_eq!(tabs.app_mut().data.header.skip(), 1, "row under the cursor, not row 0");
        assert_eq!(tabs.app_mut().data.column_names, vec!["alpha", "1"]);
        assert_eq!(tabs.app_mut().row_count(), 1);
    }

    #[test]
    fn promoting_a_row_in_a_commented_file_picks_the_row_under_the_cursor() {
        // The MetaPhlAn shape: the names come from the last `#` line, which used
        // up none of the table's rows. `H` on the first row must therefore mean
        // *that* row — not the one after it.
        let tsv = "# some command\n#clade_name\ts1\ts2\nk__Bacteria\t0.5\t0.25\nk__Archaea\t0.1\t0.2\n";
        let path = write_text_fixture("tsv", tsv);
        let mut tabs =
            Tabs::open(std::slice::from_ref(&path), HeaderSpec::default(), Store::default())
                .unwrap();
        assert_eq!(tabs.app_mut().data.column_names, vec!["clade_name", "s1", "s2"]);
        assert_eq!(
            tabs.app_mut().data.raw_row(0),
            0,
            "a comment header costs the table no row"
        );

        tabs.key(key('H'));
        assert!(tabs.step());
        assert_eq!(
            tabs.app_mut().data.column_names,
            vec!["k__Bacteria", "0.5", "0.25"],
            "the row under the cursor, not the next one"
        );
        assert_eq!(tabs.app_mut().row_count(), 1);
        // Stated outright, so the comment line no longer decides — which a
        // single `skip`/`named` pair could not have expressed.
        assert_eq!(
            tabs.app_mut().data.header,
            HeaderSpec::At { skip: 0, named: true }
        );

        // And `H` again goes back to how the file reads itself, even though the
        // row it named was the first one.
        tabs.key(key('H'));
        assert!(tabs.step());
        assert_eq!(tabs.app_mut().data.header, HeaderSpec::Auto);
        assert_eq!(tabs.app_mut().data.column_names, vec!["clade_name", "s1", "s2"]);

        // `T` states the other reading: no names at all, comments still skipped.
        tabs.key(key('T'));
        assert!(tabs.step());
        let app = tabs.app_mut();
        assert_eq!(app.data.column_names, vec!["column_1", "column_2", "column_3"]);
        assert_eq!(app.row_count(), 2, "both data rows, the comments dropped");
    }

    #[test]
    fn promoting_a_row_works_on_a_sheet_and_a_commented_file() {
        // A worksheet: H re-reads just that sheet.
        let mut tabs = Tabs::open(&[xlsx_fixture()], HeaderSpec::default(), Store::default()).unwrap();
        tabs.app_mut().handle_key(key('H'));
        assert!(tabs.step());
        assert_eq!(tabs.app_mut().data.header.skip(), 1);
        assert_eq!(tabs.app_mut().data.column_names, vec!["1", "alpha", "3.5"]);
        assert_eq!(tabs.app_mut().row_count(), 3);
        assert_eq!(tabs.tabs.len(), 2, "the other sheet is untouched");

        // A `#` preamble is skipped first, so H counts from the real content.
        let tsv = "# a comment\njunk\tjunk2\nid\tname\n1\talpha\n";
        let mut tabs =
            Tabs::open(&[write_text_fixture("tsv", tsv)], HeaderSpec::default(), Store::default()).unwrap();
        assert_eq!(tabs.app_mut().data.column_names, vec!["junk", "junk2"]);
        tabs.app_mut().handle_key(key('H'));
        assert!(tabs.step());
        assert_eq!(tabs.app_mut().data.column_names, vec!["id", "name"]);
        assert_eq!(tabs.app_mut().row_count(), 1);
    }

    #[test]
    fn no_header_names_columns_positionally() {
        let csv = "1,2,3\n4,5,6\n";
        let path = write_text_fixture("csv", csv);
        // With a header the first record names the columns and is not data.
        let headed = Dataset::load(&path).unwrap();
        assert_eq!(headed.column_names, vec!["1", "2", "3"]);
        assert_eq!(headed.nrows, 1);
        assert!(headed.header.named());
        // Without one, every record is data and the names are positional.
        let bare = Dataset::load_all(&path, HeaderSpec::NONE).unwrap().remove(0);
        assert_eq!(bare.column_names, vec!["column_1", "column_2", "column_3"]);
        assert_eq!(bare.nrows, 2);
        assert!(!bare.header.named());
        assert_eq!(bare.cell_display(0, 0).unwrap().as_deref(), Some("1"));
    }

    #[test]
    fn header_toggle_reloads_the_current_tab() {
        let csv = "id,name\n1,alpha\n2,beta\n";
        let mut tabs = Tabs::open(&[write_text_fixture("csv", csv)], HeaderSpec::default(), Store::default()).unwrap();
        assert_eq!(tabs.app_mut().row_count(), 2);

        // `T` re-reads the file with the header row as data.
        tabs.app_mut().handle_key(key('T'));
        assert!(tabs.step());
        assert!(!tabs.app_mut().data.header.named());
        assert_eq!(tabs.app_mut().data.column_names, vec!["column_1", "column_2"]);
        assert_eq!(tabs.app_mut().row_count(), 3, "the header row is now data");
        assert_eq!(tabs.tabs.len(), 1, "toggling stays in the same tab");

        // And back again.
        tabs.app_mut().handle_key(key('T'));
        assert!(tabs.step());
        assert!(tabs.app_mut().data.header.named());
        assert_eq!(tabs.app_mut().data.column_names, vec!["id", "name"]);
        assert_eq!(tabs.app_mut().row_count(), 2);
    }

    #[test]
    fn header_state_is_shown_and_declined_where_it_cannot_apply() {
        // The status bar flags a headerless view.
        let csv = "1,2\n3,4\n";
        let path = write_text_fixture("csv", csv);
        let mut bare = App::new(Dataset::load_all(&path, HeaderSpec::NONE).unwrap().remove(0));
        let text = buffer_text(&mut bare, 100, 20);
        assert!(text.contains("no header"), "missing marker: {text}");
        let mut headed = App::new(Dataset::load(&path).unwrap());
        assert!(!buffer_text(&mut headed, 100, 20).contains("no header"));

        // Parquet carries its own names, so there is nothing to toggle.
        let mut tabs = Tabs::open(&[fixture()], HeaderSpec::default(), Store::default()).unwrap();
        tabs.app_mut().handle_key(key('T'));
        assert!(tabs.step());
        let msg = tabs.app_mut().status_msg.clone().unwrap_or_default();
        assert!(msg.contains("parquet"), "unexpected message: {msg}");
        assert_eq!(tabs.app_mut().data.column_names, vec!["id", "name", "score"]);

        // In a transposed view the file's own layout is not what is on screen.
        let mut app = App::new(Dataset::load(&write_text_fixture("csv", csv)).unwrap());
        app.is_transposed = true;
        app.handle_key(key('T'));
        assert!(!app.toggle_header);
        assert!(app.status_msg.unwrap().contains("press t first"));
    }

    #[test]
    fn no_header_still_skips_a_comment_block() {
        // The `#` preamble is dropped either way; only the first data line moves.
        let tsv = "# generated by something\nid\tname\n1\talpha\n";
        let path = write_text_fixture("tsv", tsv);
        let headed = Dataset::load(&path).unwrap();
        assert_eq!(headed.column_names, vec!["id", "name"]);
        assert_eq!(headed.nrows, 1);

        let bare = Dataset::load_all(&path, HeaderSpec::NONE).unwrap().remove(0);
        assert_eq!(bare.column_names, vec!["column_1", "column_2"]);
        assert_eq!(bare.nrows, 2, "the id/name line is data now");
        assert_eq!(bare.cell_display(0, 0).unwrap().as_deref(), Some("id"));
    }

    #[test]
    fn xlsx_header_can_be_turned_off_per_sheet() {
        let book = xlsx_fixture();
        let sheets = Dataset::load_all(&book, HeaderSpec::NONE).unwrap();
        let numbers = &sheets[0];
        assert_eq!(
            numbers.column_names,
            vec!["column_1", "column_2", "column_3"]
        );
        // The header row is data now, so the id column is text, not Int64.
        assert_eq!(numbers.nrows, 5);
        assert_eq!(numbers.column_types[0], "Utf8");
        assert_eq!(numbers.cell_display(0, 0).unwrap().as_deref(), Some("id"));
        assert!(!numbers.header.named());

        // Toggling a workbook tab reloads just that sheet, keeping the others.
        let mut tabs = Tabs::open(&[book], HeaderSpec::default(), Store::default()).unwrap();
        tabs.app_mut().handle_key(code(KeyCode::Tab));
        assert!(tabs.step());
        tabs.app_mut().handle_key(key('T'));
        assert!(tabs.step());
        assert_eq!(tabs.tabs.len(), 2, "the other sheet's tab survives");
        assert_eq!(tabs.current, 1);
        assert!(!tabs.app_mut().data.header.named());
        assert_eq!(tabs.app_mut().data.column_names, vec!["column_1", "column_2"]);
        assert!(tabs.app_mut().data.label.ends_with("[Dates]"));
        // The untouched sheet still has its header.
        tabs.app_mut().handle_key(code(KeyCode::Tab));
        assert!(tabs.step());
        assert!(tabs.app_mut().data.header.named());
    }

    #[test]
    fn xlsx_opens_one_tab_per_sheet() {
        let mut tabs = Tabs::open(&[xlsx_fixture()], HeaderSpec::default(), Store::default()).unwrap();
        assert_eq!(tabs.tabs.len(), 2, "the blank sheet earns no tab");
        let strip = tabs.strip();
        assert!(
            strip.labels[0].ends_with("[Numbers]") && strip.labels[1].ends_with("[Dates]"),
            "sheets should label their tabs: {:?}",
            strip.labels
        );
        // The sheets are ordinary tabs: Tab moves between them.
        tabs.app_mut().handle_key(code(KeyCode::Tab));
        assert!(tabs.step());
        assert_eq!(tabs.current, 1);
        assert_eq!(tabs.app_mut().data.column_names, vec!["day", "moment"]);
    }

    #[test]
    fn xlsx_columns_keep_their_excel_types() {
        let ds = Dataset::load(&xlsx_fixture()).unwrap(); // first sheet: Numbers
        assert_eq!(ds.column_names, vec!["id", "name", "score"]);
        assert_eq!(ds.nrows, 4);
        // Excel reports every number as a float; whole ones read back as Int64,
        // so an id column shows `1` rather than `1.0`.
        assert_eq!(ds.column_types, vec!["Int64", "Utf8", "Float64"]);
        assert_eq!(ds.cell_display(0, 0).unwrap().as_deref(), Some("1"));
        assert_eq!(ds.cell_display(2, 1).unwrap().as_deref(), Some("10.25"));
        assert!(ds.is_numeric(0) && ds.is_numeric(2));
        // A blank cell and an Excel error cell both read as null (shown as NA).
        assert!(ds.is_null(1, 3), "blank cell should be null");
        assert!(ds.is_null(2, 3), "#DIV/0! should be null");
    }

    #[test]
    fn xlsx_dates_become_real_date_columns() {
        let sheets = Dataset::load_all(&xlsx_fixture(), HeaderSpec::default()).unwrap();
        let dates = &sheets[1];
        // Date-formatted cells become dates; a time of day promotes to timestamp.
        assert_eq!(dates.column_types, vec!["Date32", "Timestamp(ms)"]);
        assert_eq!(dates.cell_display(0, 0).unwrap().as_deref(), Some("2024-01-31"));
        assert_eq!(
            dates.cell_display(1, 0).unwrap().as_deref(),
            Some("2024-01-31T12:30:00")
        );
        // Dates are not numeric, so `%` correctly declines them.
        assert!(!dates.is_numeric(0) && !dates.is_numeric(1));
    }

    #[test]
    fn xlsx_sheet_behaves_like_a_normal_table() {
        let mut app = App::new(Dataset::load(&xlsx_fixture()).unwrap());

        // Sorting a float column orders it numerically, nulls aside.
        app.selected_pos = 2;
        app.handle_key(key('s'));
        let ordered: Vec<f64> = app
            .data
            .cells(2, &app.view_rows(10))
            .unwrap()
            .into_iter()
            .flatten()
            .map(|v| v.parse::<f64>().unwrap())
            .collect();
        assert_eq!(ordered, vec![0.5, 3.5, 10.25], "sorted ascending by score");

        // Filtering streams the in-memory sheet like any other backend.
        app.handle_key(key('&'));
        type_str(&mut app, "beta");
        app.handle_key(code(KeyCode::Enter));
        assert_eq!(app.row_count(), 1);
        assert_eq!(app.data.cell_display(1, app.selected_orig()).unwrap().as_deref(), Some("beta"));

        // And it transposes like any other table.
        app.handle_key(code(KeyCode::Esc));
        let view = transposed_view(&app).unwrap();
        assert!(view.is_transposed);
        assert_eq!(view.data.ncols, 5, "field column plus one per record");
    }

    #[test]
    fn workbook_detected_from_its_magic_number() {
        // No useful extension: the ZIP container still identifies a workbook.
        let book = xlsx_fixture();
        let mut path = std::env::temp_dir();
        path.push(format!("{}.dat", book.file_stem().unwrap().to_string_lossy()));
        std::fs::copy(&book, &path).unwrap();
        let sheets = Dataset::load_all(&path, HeaderSpec::default()).unwrap();
        assert_eq!(sheets.len(), 2);
        assert_eq!(sheets[0].column_names, vec!["id", "name", "score"]);
    }

    fn buffer_text(app: &mut App, w: u16, h: u16) -> String {
        buffer_text_tabs(app, &ui::TabStrip::default(), w, h)
    }

    /// One row of the rendered frame, trailing blanks trimmed.
    fn buffer_line(app: &mut App, w: u16, h: u16, y: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal
            .draw(|f| ui::render(f, app, &ui::TabStrip::default(), None).unwrap())
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..w)
            .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    /// Render `app` with a wizard banner on the bottom line.
    fn buffer_text_banner(app: &mut App, banner: &str, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal
            .draw(|f| ui::render(f, app, &ui::TabStrip::default(), Some(banner)).unwrap())
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    /// Render `app` as the visible view of `tabs` (an empty strip = one tab).
    fn buffer_text_tabs(app: &mut App, tabs: &ui::TabStrip, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal
            .draw(|f| ui::render(f, app, tabs, None).unwrap())
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::from(KeyCode::Char(c))
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn code(c: KeyCode) -> KeyEvent {
        KeyEvent::from(c)
    }

    fn type_str(app: &mut App, s: &str) {
        for c in s.chars() {
            app.handle_key(key(c));
        }
    }

    #[test]
    fn loads_metadata() {
        let ds = Dataset::load(&fixture()).unwrap();
        assert_eq!(ds.nrows, 50);
        assert_eq!(ds.ncols, 3);
        assert_eq!(ds.column_names, vec!["id", "name", "score"]);
        assert_eq!(ds.column_types, vec!["Int64", "Utf8", "Int64"]);
        assert!(ds.is_null(2, 0), "row 0 score should be null");
        assert!(!ds.is_null(2, 1), "row 1 score should be present");
    }

    // 20_000 rows spans three 8_192-row chunks, exercising chunk boundaries,
    // the LRU cache, and the last partial chunk.
    const BIG: i64 = 20_000;

    #[test]
    fn large_parquet_spans_chunks() {
        let ds = Dataset::load(&big_parquet(BIG)).unwrap();
        assert_eq!(ds.nrows, BIG as usize);
        // Cells across every chunk, including boundaries and the last row.
        for row in [0usize, 8191, 8192, 8193, 16384, 19999] {
            assert_eq!(
                ds.cell_display(0, row).unwrap().as_deref(),
                Some(row.to_string().as_str())
            );
        }
        // A window straddling a chunk boundary.
        let vals = ds.cells(0, &[8191, 8192, 8193]).unwrap();
        assert_eq!(
            vals,
            vec![Some("8191".into()), Some("8192".into()), Some("8193".into())]
        );
        // Sorting reads the full column across all chunks.
        let all: Vec<usize> = (0..BIG as usize).collect();
        let sorted = ds.sort_indices(&all, 0, true, || false).unwrap().unwrap();
        assert_eq!(sorted[0], 19999);
        assert_eq!(*sorted.last().unwrap(), 0);
    }

    #[test]
    fn identity_view_needs_no_materialised_index() {
        // With no filter/sort the view maps positions to rows directly, so a
        // huge file costs nothing for row bookkeeping.
        let mut app = App::new(Dataset::load(&big_parquet(BIG)).unwrap());
        assert_eq!(app.row_count(), BIG as usize);
        assert_eq!(app.orig_row(12_345), 12_345);
        // Filtering materialises a subset; clearing returns to the identity view.
        app.handle_key(key('&'));
        type_str(&mut app, "^100$");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(app.row_count(), 1);
        assert_eq!(app.selected_orig(), 100);
        app.handle_key(KeyEvent::from(KeyCode::Esc));
        assert_eq!(app.row_count(), BIG as usize);
        assert_eq!(app.orig_row(777), 777);
    }

    #[test]
    fn large_csv_spans_chunks_and_filters() {
        let ds = Dataset::load(&big_csv(BIG as usize)).unwrap();
        assert_eq!(ds.nrows, BIG as usize, "row count from the offset index");
        assert_eq!(ds.cell_display(0, 8192).unwrap().as_deref(), Some("8192"));
        assert_eq!(ds.cell_display(0, 19999).unwrap().as_deref(), Some("19999"));
        // Filtering streams every chunk; the match lives in the third one.
        let hits = ds
            .filter_rows(&[0, 1], &regex::Regex::new("^18000$").unwrap(), || false)
            .unwrap()
            .unwrap();
        assert_eq!(hits, vec![18000]);
    }

    #[test]
    fn cancellation_aborts_heavy_ops() {
        let ds = Dataset::load(&big_parquet(BIG)).unwrap();
        let never = || false;
        let always = || true;

        // A firing cancel aborts sort and filter before they finish.
        let all: Vec<usize> = (0..BIG as usize).collect();
        assert!(ds.sort_indices(&all, 0, true, always).unwrap().is_none());
        assert!(ds
            .filter_rows(&[0, 1], &regex::Regex::new("x").unwrap(), always)
            .unwrap()
            .is_none());

        // Search for a value that exists near the end: found normally, but a
        // firing cancel aborts before reaching it.
        let re = regex::Regex::new("^19999$").unwrap();
        assert_eq!(
            ds.find_match(&re, BIG as usize, &[0, 1], |i| i, 0, 0, true, Some(0), never),
            Some((19999, 0)),
        );
        assert!(ds
            .find_match(&re, BIG as usize, &[0, 1], |i| i, 0, 0, true, Some(0), always)
            .is_none());
    }

    #[test]
    fn csv_index_handles_quoted_newlines() {
        // A quoted field spanning a newline must stay a single record.
        let csv = "id,note\n1,\"line one\nline two\"\n2,plain\n";
        let ds = Dataset::load(&write_text_fixture("csv", csv)).unwrap();
        assert_eq!(ds.nrows, 2, "embedded newline must not split the record");
        assert_eq!(
            ds.cell_display(1, 0).unwrap().as_deref(),
            Some("line one\nline two")
        );
        assert_eq!(ds.cell_display(0, 1).unwrap().as_deref(), Some("2"));
    }

    #[test]
    fn csv_skips_comment_preamble() {
        // Pure comment preamble: the header is the first non-`#` line.
        let csv = "# generated by tool\n# version 1.2\nid,name,score\n1,alpha,10\n2,beta,20\n";
        let ds = Dataset::load(&write_text_fixture("csv", csv)).unwrap();
        assert_eq!(ds.column_names, vec!["id", "name", "score"]);
        assert_eq!(ds.column_types, vec!["Int64", "Utf8", "Int64"]);
        assert_eq!(ds.nrows, 2);
        assert_eq!(ds.cell_display(1, 0).unwrap().as_deref(), Some("alpha"));
    }

    #[test]
    fn tsv_metaphlan_style_hash_header() {
        // MetaPhlAn: preamble comments, then a `#`-prefixed header line whose
        // column count matches the data, then data rows.
        let tsv = concat!(
            "#mpa_vJan21\n",
            "#/usr/bin/metaphlan input.fastq --input_type fastq\n",
            "#clade_name\tNCBI_tax_id\trelative_abundance\n",
            "k__Bacteria\t2\t99.5\n",
            "k__Archaea\t2157\t0.5\n",
        );
        let ds = Dataset::load(&write_text_fixture("tsv", tsv)).unwrap();
        assert_eq!(
            ds.column_names,
            vec!["clade_name", "NCBI_tax_id", "relative_abundance"]
        );
        assert_eq!(ds.column_types, vec!["Utf8", "Int64", "Float64"]);
        assert_eq!(ds.nrows, 2, "the `#` header line is not counted as data");
        assert_eq!(ds.cell_display(0, 0).unwrap().as_deref(), Some("k__Bacteria"));
        assert_eq!(ds.cell_display(1, 1).unwrap().as_deref(), Some("2157"));
    }

    /// Write `content` gzipped to a uniquely-named temp file.
    fn write_gzip_fixture(name: &str, content: &str) -> PathBuf {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!("lambris_test_gz_{n}.{name}.gz"));
        let file = std::fs::File::create(&path).unwrap();
        let mut encoder = GzEncoder::new(file, Compression::fast());
        encoder.write_all(content.as_bytes()).unwrap();
        encoder.finish().unwrap();
        path
    }

    #[test]
    fn reads_a_gzipped_delimited_file() {
        // The delimiter comes from the extension under the `.gz`.
        let tsv = "id\tname\n1\talpha\n2\tbeta\n";
        let ds = Dataset::load(&write_gzip_fixture("tsv", tsv)).unwrap();
        assert_eq!(ds.column_names, vec!["id", "name"]);
        assert_eq!(ds.column_types, vec!["Int64", "Utf8"]);
        assert_eq!(ds.nrows, 2);
        assert_eq!(ds.cell_display(1, 1).unwrap().as_deref(), Some("beta"));

        let csv = "a,b\n1,2\n";
        let ds = Dataset::load(&write_gzip_fixture("csv", csv)).unwrap();
        assert_eq!(ds.column_names, vec!["a", "b"]);

        // With nothing useful under the `.gz`, the delimiter is sniffed from
        // the expanded text.
        let ds = Dataset::load(&write_gzip_fixture("dat", tsv)).unwrap();
        assert_eq!(ds.column_names, vec!["id", "name"]);
    }

    #[test]
    fn a_gzipped_file_keeps_every_other_behaviour() {
        // Big enough to span chunks, so it exercises the byte-offset index over
        // the expanded copy rather than a single read.
        let mut text = String::from("id,name\n");
        for i in 0..BIG {
            text.push_str(&format!("{i},r{i}\n"));
        }
        let path = write_gzip_fixture("csv", &text);
        let ds = Dataset::load(&path).unwrap();
        assert_eq!(ds.nrows, BIG as usize);
        for row in [0usize, 8191, 8192, 19_999] {
            assert_eq!(
                ds.cell_display(0, row).unwrap().as_deref(),
                Some(row.to_string().as_str()),
                "row {row} across a chunk boundary"
            );
        }
        // Filtering streams the expanded file the same way.
        let hits = ds
            .filter_rows(&[0, 1], &regex::Regex::new("^18000$").unwrap(), || false)
            .unwrap()
            .unwrap();
        assert_eq!(hits, vec![18000]);

        // The label and the pattern binding are the name the user typed, not
        // the temporary copy's.
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(ds.label, name);
        assert!(name.ends_with(".gz"));
    }

    #[test]
    fn a_gzipped_file_reads_all_of_its_members() {
        // bgzip writes a series of gzip streams, and `cat a.gz b.gz` is a valid
        // file: reading only the first member would truncate the table.
        use flate2::write::GzEncoder;
        use flate2::Compression;
        let mut bytes = Vec::new();
        for part in ["id\n1\n2\n", "3\n4\n"] {
            let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
            encoder.write_all(part.as_bytes()).unwrap();
            bytes.extend(encoder.finish().unwrap());
        }
        let mut path = std::env::temp_dir();
        path.push("lambris_test_multimember.csv.gz");
        std::fs::write(&path, bytes).unwrap();

        let ds = Dataset::load(&path).unwrap();
        assert_eq!(ds.nrows, 4, "every member should be read");
        assert_eq!(ds.cell_display(0, 3).unwrap().as_deref(), Some("4"));
    }

    #[test]
    fn a_gzipped_file_keeps_the_comment_and_header_handling() {
        // The MetaPhlAn shape, gzipped: the last `#` line is the header.
        let tsv = "# some command\n#clade_name\ts1\ts2\nk__Bacteria\t0.5\t0.25\n";
        let path = write_gzip_fixture("tsv", tsv);
        let ds = Dataset::load(&path).unwrap();
        assert_eq!(ds.column_names, vec!["clade_name", "s1", "s2"]);
        assert_eq!(ds.nrows, 1);

        // And re-reading the file — `T` here — expands it again rather than
        // losing track of where the text went.
        let mut tabs =
            Tabs::open(std::slice::from_ref(&path), HeaderSpec::default(), Store::default())
                .unwrap();
        tabs.key(key('T'));
        assert!(tabs.step());
        let app = tabs.app_mut();
        assert!(!app.data.header.named());
        assert_eq!(app.data.column_names, vec!["column_1", "column_2", "column_3"]);
        assert_eq!(app.row_count(), 1, "the comment block is still skipped");
        assert_eq!(app.data.cell_display(0, 0).unwrap().as_deref(), Some("k__Bacteria"));
    }

    #[test]
    fn the_expanded_copy_is_cleaned_up() {
        let path = write_gzip_fixture("csv", "a\n1\n");
        let temporary = || -> Vec<PathBuf> {
            std::fs::read_dir(std::env::temp_dir())
                .unwrap()
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .map(|n| n.to_string_lossy().starts_with("lambris-"))
                        .unwrap_or(false)
                })
                .collect()
        };
        // Only this file's copy: other tests expand their own at the same time,
        // and counting all of them would race them.
        let mine = path.file_name().unwrap().to_string_lossy().into_owned();
        let copies = || temporary().iter().filter(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().ends_with(&mine))
                .unwrap_or(false)
        }).count();
        assert_eq!(copies(), 0);
        {
            let ds = Dataset::load(&path).unwrap();
            assert_eq!(ds.nrows, 1);
            assert_eq!(copies(), 1, "an expanded copy exists while open");
        }
        assert_eq!(copies(), 0, "and goes when the dataset does");
    }

    #[test]
    fn loads_csv_with_inferred_types_and_nulls() {
        let csv = "id,name,score\n1,alpha,10\n2,beta,\n3,gamma,30\n";
        let ds = Dataset::load(&write_text_fixture("csv", csv)).unwrap();
        assert_eq!(ds.nrows, 3);
        assert_eq!(ds.ncols, 3);
        assert_eq!(ds.column_names, vec!["id", "name", "score"]);
        // Types are inferred from the data.
        assert_eq!(ds.column_types, vec!["Int64", "Utf8", "Int64"]);
        // The empty score cell (row index 1) is a null.
        assert!(ds.is_null(2, 1), "empty field should parse as null");

        // The Arrow pipeline (formatters, NA rendering) works on CSV too.
        let mut app = App::new(ds);
        let text = buffer_text(&mut app, 60, 10);
        assert!(text.contains("alpha"), "cell missing: {text}");
        assert!(text.contains("NA"), "null not rendered as NA: {text}");
    }

    #[test]
    fn loads_tsv_by_extension() {
        let tsv = "id\tname\n1\talpha\n2\tbeta\n";
        let ds = Dataset::load(&write_text_fixture("tsv", tsv)).unwrap();
        assert_eq!(ds.ncols, 2, "tab delimiter split into two columns");
        assert_eq!(ds.nrows, 2);
        assert_eq!(ds.column_names, vec!["id", "name"]);
    }

    #[test]
    fn detects_delimiter_for_unknown_extension() {
        // A .txt file with tab-separated content must be sniffed as TSV;
        // if it were parsed as CSV the whole line would be a single column.
        let tsv = "a\tb\tc\n1\t2\t3\n";
        let ds = Dataset::load(&write_text_fixture("txt", tsv)).unwrap();
        assert_eq!(ds.ncols, 3, "sniffed tab delimiter");
        assert_eq!(ds.nrows, 1);
    }

    #[test]
    fn renders_header_and_cells() {
        let mut app = App::new(Dataset::load(&fixture()).unwrap());
        let text = buffer_text(&mut app, 80, 20);
        assert!(text.contains("name"), "column header missing: {text}");
        assert!(text.contains("item_0000"), "first cell missing: {text}");
        assert!(text.contains("50 rows"), "title dims missing: {text}");
    }

    #[test]
    fn scrolls_to_keep_selection_visible() {
        let mut app = App::new(Dataset::load(&fixture()).unwrap());
        // Jump to the last row; the viewport must follow it.
        app.handle_key(KeyEvent::from(KeyCode::Char('G')));
        assert_eq!(app.selected_row, 49);
        let text = buffer_text(&mut app, 80, 10);
        assert!(text.contains("item_0049"), "last row not visible: {text}");
        assert!(app.row_offset > 0, "viewport did not scroll");
    }

    #[test]
    fn navigation_clamps_at_bounds() {
        let mut app = App::new(Dataset::load(&fixture()).unwrap());
        app.handle_key(KeyEvent::from(KeyCode::Up)); // above row 0
        assert_eq!(app.selected_row, 0);
        app.handle_key(KeyEvent::from(KeyCode::Left)); // left of col 0
        assert_eq!(app.selected_pos, 0);
        app.handle_key(KeyEvent::from(KeyCode::Char('$')));
        assert_eq!(app.selected_pos, 2);
        app.handle_key(KeyEvent::from(KeyCode::Right)); // past last col
        assert_eq!(app.selected_pos, 2);
    }

    #[test]
    fn help_hints_shown_and_info_toggles() {
        let mut app = App::new(Dataset::load(&fixture()).unwrap());
        // Default: command hints on the bottom line, no column type on screen.
        let text = buffer_text(&mut app, 100, 20);
        assert!(text.contains("q quit"), "command hints missing: {text}");
        assert!(!text.contains("Int64"), "type shown outside info mode: {text}");

        // Pressing `i` reveals the selected column's type and value.
        app.handle_key(key('i'));
        assert!(app.show_info);
        let text = buffer_text(&mut app, 100, 20);
        assert!(text.contains("id: Int64"), "info line missing type: {text}");
        assert!(!text.contains("q quit"), "hints should be replaced by info: {text}");

        // `i` again toggles back to the hints.
        app.handle_key(key('i'));
        assert!(!app.show_info);
    }

    #[test]
    fn held_key_accelerates_scroll() {
        let t0 = Instant::now();

        // Ten rapid `j` presses (10ms apart) — inside the repeat window.
        let mut fast = App::new(Dataset::load(&fixture()).unwrap());
        for i in 0..10 {
            fast.handle_key_at(key('j'), t0 + Duration::from_millis(i * 10));
        }

        // Ten slow presses (1s apart) — never treated as held.
        let mut slow = App::new(Dataset::load(&fixture()).unwrap());
        for i in 0..10 {
            slow.handle_key_at(key('j'), t0 + Duration::from_secs(i));
        }

        assert_eq!(slow.selected_row, 10, "slow presses move exactly one row each");
        assert!(
            fast.selected_row > slow.selected_row,
            "held key should scroll further: fast={} slow={}",
            fast.selected_row,
            slow.selected_row,
        );
    }

    #[test]
    fn goto_line_jumps_to_row() {
        let mut app = App::new(Dataset::load(&fixture()).unwrap());
        app.handle_key(key(':'));
        type_str(&mut app, "27");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(app.selected_orig(), 26, "line 27 is original row 26");

        // Non-digits are rejected at the prompt, so this stays empty and no-ops.
        app.handle_key(key(':'));
        type_str(&mut app, "abc");
        assert_eq!(app.input, "");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(app.selected_orig(), 26, "selection unchanged");
    }

    #[test]
    fn goto_line_out_of_view_reports() {
        let mut app = App::new(Dataset::load(&fixture()).unwrap());
        // Filter to item_004x, then ask for a line that was filtered out.
        app.handle_key(key('&'));
        type_str(&mut app, "item_004");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        app.handle_key(key(':'));
        type_str(&mut app, "3");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        assert!(app.status_msg.as_deref().unwrap().contains("not in current view"));
    }

    #[test]
    fn sort_cycles_asc_desc_none() {
        use crate::app::SortDir;
        let mut app = App::new(Dataset::load(&fixture()).unwrap());
        // Selected column 0 = id (already ascending).
        app.handle_key(key('s'));
        assert_eq!(app.sort.unwrap().dir, SortDir::Asc);
        assert_eq!(app.orig_row(0), 0);

        app.handle_key(key('s'));
        assert_eq!(app.sort.unwrap().dir, SortDir::Desc);
        assert_eq!(app.orig_row(0), 49, "descending puts the largest id first");

        app.handle_key(key('s'));
        assert!(app.sort.is_none(), "third press clears the sort");
        assert_eq!(app.orig_row(0), 0, "natural order restored");
    }

    #[test]
    fn sort_keeps_cursor_on_record_and_handles_nulls() {
        let mut app = App::new(Dataset::load(&fixture()).unwrap());
        // Park the cursor on original row 4 (line 5).
        app.handle_key(key(':'));
        type_str(&mut app, "5");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(app.selected_orig(), 4);

        // Sort descending by id; the cursor should follow row 4.
        app.handle_key(key('s'));
        app.handle_key(key('s'));
        assert_eq!(app.selected_orig(), 4, "cursor tracks the record");

        // Sorting the nullable `score` column must not panic.
        app.handle_key(key('$')); // select score
        app.handle_key(key('s'));
        assert_eq!(app.row_count(), 50);
        assert!(app.sort.is_some());
    }

    #[test]
    fn sort_composes_with_filter() {
        let mut app = App::new(Dataset::load(&fixture()).unwrap());
        app.handle_key(key('&'));
        type_str(&mut app, "item_004"); // rows 40..49
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        app.handle_key(key('s')); // asc by id
        app.handle_key(key('s')); // desc by id
        assert_eq!(app.row_count(), 10);
        assert_eq!(app.orig_row(0), 49, "sort applies within the filtered set");
        // Clearing the filter keeps the sort applied over all rows.
        app.handle_key(KeyEvent::from(KeyCode::Esc));
        assert_eq!(app.row_count(), 50);
        assert_eq!(app.orig_row(0), 49);
    }

    #[test]
    fn freeze_toggles_at_selected_boundary() {
        let mut app = App::new(Dataset::load(&fixture()).unwrap());
        app.handle_key(key('f'));
        assert_eq!(app.frozen_cols, 1);
        app.handle_key(key('f'));
        assert_eq!(app.frozen_cols, 0, "same boundary unfreezes");
        app.handle_key(key('$')); // select last column (index 2)
        app.handle_key(key('f'));
        assert_eq!(app.frozen_cols, 3, "freezes through the selected column");
    }

    #[test]
    fn frozen_column_stays_visible_when_scrolled() {
        let mut app = App::new(Dataset::load(&fixture()).unwrap());
        app.handle_key(key('f')); // freeze id
        app.handle_key(key('$')); // jump to score, forcing a horizontal scroll
        let text = buffer_text(&mut app, 24, 10);
        assert!(text.contains("id"), "frozen column dropped: {text}");
        assert!(text.contains("│"), "freeze divider missing: {text}");
        assert!(text.contains("score"), "selected column not visible: {text}");
    }

    #[test]
    fn transpose_builds_a_real_table() {
        // A features × samples matrix: first column labels, rest numeric.
        let csv = "gene,s1,s2\ng1,10,20\ng2,3,4\n";
        let ds = Dataset::load(&write_text_fixture("csv", csv)).unwrap();
        let t = ds.transpose(&[0, 1], &[0, 1, 2]).unwrap();

        // Columns become the field column + one per record (titled by gene).
        assert_eq!(t.column_names, vec!["field", "g1", "g2"]);
        assert_eq!(t.nrows, 2); // s1, s2
        assert_eq!(t.ncols, 3);
        assert_eq!(t.column_types[0], "Utf8"); // field-name column
        assert_eq!(t.cell_display(0, 0).unwrap().as_deref(), Some("s1"));
        assert_eq!(t.cell_display(0, 1).unwrap().as_deref(), Some("s2"));
        // Record columns get a real (numeric) type inferred from their values.
        assert_eq!(t.column_types[1], "Int64");
        assert_eq!(t.column_types[2], "Int64");
        assert_eq!(t.cell_display(1, 0).unwrap().as_deref(), Some("10")); // g1 / s1
        assert_eq!(t.cell_display(1, 1).unwrap().as_deref(), Some("20")); // g1 / s2
        assert_eq!(t.cell_display(2, 0).unwrap().as_deref(), Some("3")); // g2 / s1
        assert_eq!(t.cell_display(2, 1).unwrap().as_deref(), Some("4")); // g2 / s2
    }

    #[test]
    fn transposed_view_behaves_like_a_normal_table() {
        let csv = "gene,s1,s2\ng1,10,5\ng2,3,20\n";
        let mut app = App::new(Dataset::load(&write_text_fixture("csv", csv)).unwrap());
        app.handle_key(key('t'));
        assert!(app.transpose_request, "`t` requests a transposed view");

        // Build the view as the main loop would.
        let view_app = transposed_view(&app).unwrap();
        let mut tv = view_app;
        assert!(tv.is_transposed);

        // All the usual commands act on the transposed columns directly.
        tv.handle_key(key('l')); // select record column `g1`
        assert_eq!(tv.selected_pos, 1);
        assert_eq!(tv.selected_col(), 1, "nothing moved, so the two agree");
        tv.handle_key(key('s')); // sort by it
        assert_eq!(tv.sort.unwrap().col, 1);
        tv.handle_key(key('%')); // numeric-style it
        assert!(tv.num_styles.contains_key(&1));

        // `t` (or Esc) requests leaving the transposed view.
        tv.handle_key(key('t'));
        assert!(tv.exit_transpose);
    }

    #[test]
    fn transpose_needs_at_least_two_columns() {
        let csv = "only\n1\n2\n3\n";
        let app = App::new(Dataset::load(&write_text_fixture("csv", csv)).unwrap());
        assert!(
            transposed_view(&app).is_err(),
            "single-column transpose is meaningless"
        );
    }

    #[test]
    fn tabs_cycle_with_tab_and_back_tab() {
        let mut tabs = Tabs::open(&[fixture(), fixture(), fixture()], HeaderSpec::default(), Store::default()).unwrap();
        assert_eq!(tabs.tabs.len(), 3);
        assert_eq!(tabs.current, 0);

        tabs.app_mut().handle_key(code(KeyCode::Tab));
        assert!(tabs.step());
        assert_eq!(tabs.current, 1);

        // Wraps past the last tab, and BackTab walks the other way.
        tabs.app_mut().handle_key(code(KeyCode::Tab));
        assert!(tabs.step());
        tabs.app_mut().handle_key(code(KeyCode::Tab));
        assert!(tabs.step());
        assert_eq!(tabs.current, 0, "Tab wraps around to the first tab");
        tabs.app_mut().handle_key(code(KeyCode::BackTab));
        assert!(tabs.step());
        assert_eq!(tabs.current, 2, "BackTab wraps back to the last tab");
    }

    #[test]
    fn each_tab_keeps_its_own_view_state() {
        let mut tabs = Tabs::open(&[fixture(), fixture()], HeaderSpec::default(), Store::default()).unwrap();

        // Transpose tab 0.
        tabs.app_mut().handle_key(key('t'));
        assert!(tabs.step());
        assert!(tabs.app_mut().is_transposed);

        // Tab 1 is an untouched base view; move its cursor.
        tabs.app_mut().handle_key(code(KeyCode::Tab));
        assert!(tabs.step());
        assert!(!tabs.app_mut().is_transposed, "transpose is per tab");
        tabs.app_mut().handle_key(key('j'));
        assert_eq!(tabs.app_mut().selected_row, 1);

        // Back on tab 0: still transposed, its own cursor untouched.
        tabs.app_mut().handle_key(code(KeyCode::BackTab));
        assert!(tabs.step());
        assert!(tabs.app_mut().is_transposed);
        assert_eq!(tabs.app_mut().selected_row, 0);

        // Leaving the transposed view pops only that tab's stack.
        tabs.app_mut().handle_key(key('t'));
        assert!(tabs.step());
        assert!(!tabs.app_mut().is_transposed);
        assert_eq!(tabs.tabs.len(), 2, "leaving a transposed view keeps the tab");
    }

    #[test]
    fn open_adds_a_tab_and_ctrl_w_closes_it() {
        let mut tabs = Tabs::open(&[fixture()], HeaderSpec::default(), Store::default()).unwrap();
        let other = fixture();

        tabs.app_mut().handle_key(key('o'));
        type_str(tabs.app_mut(), other.to_str().unwrap());
        tabs.app_mut().handle_key(code(KeyCode::Enter));
        assert!(tabs.step());
        assert_eq!(tabs.tabs.len(), 2);
        assert_eq!(tabs.current, 1, "the newly opened file becomes current");

        // Ctrl-W closes it and lands back on the remaining tab.
        tabs.app_mut().handle_key(ctrl('w'));
        assert!(tabs.step());
        assert_eq!(tabs.tabs.len(), 1);
        assert_eq!(tabs.current, 0);

        // Closing the last tab exits the program.
        tabs.app_mut().handle_key(ctrl('w'));
        assert!(!tabs.step(), "closing the last tab should quit");
    }

    #[test]
    fn open_reports_a_bad_path_without_adding_a_tab() {
        let mut tabs = Tabs::open(&[fixture()], HeaderSpec::default(), Store::default()).unwrap();
        tabs.app_mut().handle_key(key('o'));
        type_str(tabs.app_mut(), "/no/such/file.parquet");
        tabs.app_mut().handle_key(code(KeyCode::Enter));
        assert!(tabs.step());
        assert_eq!(tabs.tabs.len(), 1, "a failed open must not create a tab");
        let msg = tabs.app_mut().status_msg.clone().unwrap_or_default();
        assert!(msg.contains("open failed"), "unexpected message: {msg}");
    }

    #[test]
    fn open_prompt_takes_a_literal_path_and_can_be_cancelled() {
        let mut tabs = Tabs::open(&[fixture()], HeaderSpec::default(), Store::default()).unwrap();
        // The prompt shows the typed path verbatim (no regex handling).
        tabs.app_mut().handle_key(key('o'));
        type_str(tabs.app_mut(), "some/file.csv");
        let text = buffer_text(tabs.app_mut(), 100, 20);
        assert!(text.contains("open some/file.csv"), "prompt missing: {text}");

        // Esc abandons it, leaving the tab set alone.
        tabs.app_mut().handle_key(code(KeyCode::Esc));
        assert!(tabs.step());
        assert_eq!(tabs.tabs.len(), 1);
    }

    #[test]
    fn tab_strip_lists_the_open_files() {
        let paths = vec![fixture(), fixture()];
        let mut tabs = Tabs::open(&paths, HeaderSpec::default(), Store::default()).unwrap();
        let strip = tabs.strip();
        assert_eq!(strip.labels.len(), 2);
        let text = buffer_text_tabs(tabs.app_mut(), &strip, 120, 20);
        for path in &paths {
            let name = path.file_name().unwrap().to_string_lossy();
            assert!(text.contains(name.as_ref()), "tab {name} missing: {text}");
        }
        // A single tab keeps the plain title line with the row/column counts.
        let mut one = App::new(Dataset::load(&paths[0]).unwrap());
        let text = buffer_text(&mut one, 120, 20);
        assert!(text.contains("50 rows"), "single-tab title missing: {text}");
    }

    #[test]
    fn tab_strip_windows_to_keep_the_active_tab_visible() {
        // More tabs than fit: the strip scrolls so the active one is on screen.
        let paths: Vec<PathBuf> = (0..8).map(|_| fixture()).collect();
        let mut tabs = Tabs::open(&paths, HeaderSpec::default(), Store::default()).unwrap();
        tabs.current = 7;
        let strip = tabs.strip();
        let text = buffer_text_tabs(tabs.app_mut(), &strip, 40, 20);
        let active = paths[7].file_name().unwrap().to_string_lossy();
        assert!(text.contains(active.as_ref()), "active tab hidden: {text}");
        assert!(text.contains('\u{2039}'), "no overflow marker: {text}");
    }

    #[test]
    fn percent_enables_numeric_style_and_decimals() {
        let csv = "id,name,val\n1,alpha,3.14\n2,beta,100.5\n3,gamma,0.007\n";
        let mut app = App::new(Dataset::load(&write_text_fixture("csv", csv)).unwrap());
        app.handle_key(key('$')); // select `val` (col 2)
        assert_eq!(app.selected_pos, 2);

        app.handle_key(key('%'));
        let st = app.num_styles.get(&2).copied().expect("numeric style set");
        assert!(st.align && st.log);

        app.handle_key(key('>')); // fix to 3 decimals
        assert_eq!(app.num_styles[&2].decimals, Some(3));
        let text = buffer_text(&mut app, 50, 10);
        assert!(text.contains("3.140"), "fixed decimals not applied: {text}");
        assert!(text.contains("100.500"), "fixed decimals not applied: {text}");

        // `%` is rejected on a non-numeric column.
        app.handle_key(key('h')); // move to `name` (col 1)
        assert_eq!(app.selected_pos, 1);
        app.handle_key(key('%'));
        assert!(!app.num_styles.contains_key(&1));
        assert!(app.status_msg.as_deref().unwrap().contains("not numeric"));
    }

    #[test]
    fn decimals_align_without_log_colour() {
        let csv = "v\n1.5\n22.25\n";
        let mut app = App::new(Dataset::load(&write_text_fixture("csv", csv)).unwrap());
        // `>` alone turns on alignment and fixed decimals, but not colouring.
        app.handle_key(key('>'));
        let st = app.num_styles.get(&0).copied().expect("style set");
        assert!(st.align);
        assert!(!st.log, "`<`/`>` must not enable log colour");
        assert_eq!(st.decimals, Some(3));
    }

    #[test]
    fn hash_toggles_line_number_gutter() {
        let mut app = App::new(Dataset::load(&fixture()).unwrap());
        assert!(app.show_line_numbers);
        // The gutter header "#" is the only source of '#' on screen.
        let shown = buffer_text(&mut app, 40, 10);
        assert!(shown.contains('#'), "gutter header missing: {shown}");

        app.handle_key(key('#'));
        assert!(!app.show_line_numbers);
        let hidden = buffer_text(&mut app, 40, 10);
        assert!(!hidden.contains('#'), "gutter still present: {hidden}");
        assert_ne!(shown, hidden);

        app.handle_key(key('#'));
        assert!(app.show_line_numbers);
    }

    #[test]
    fn renders_null_as_na() {
        let mut app = App::new(Dataset::load(&fixture()).unwrap());
        // Row 0's score is null; "NA" must appear in the rendered buffer.
        let text = buffer_text(&mut app, 80, 20);
        assert!(text.contains("NA"), "null cell not rendered as NA: {text}");
    }

    #[test]
    fn search_jumps_to_match() {
        let mut app = App::new(Dataset::load(&fixture()).unwrap());
        app.handle_key(key('/'));
        type_str(&mut app, "item_0042");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(app.selected_orig(), 42);
        assert_eq!(app.selected_pos, 1, "should land on the name column");
        assert_eq!(app.search.as_ref().unwrap().scope, None, "global search is unscoped");
    }

    #[test]
    fn column_search_scoped_and_cycles_within_column() {
        let mut app = App::new(Dataset::load(&fixture()).unwrap());
        app.handle_key(KeyEvent::from(KeyCode::Right)); // select `name` (col 1)
        app.handle_key(key('-'));
        type_str(&mut app, "item_004"); // matches name in rows 40..49
        app.handle_key(KeyEvent::from(KeyCode::Enter));

        assert_eq!(app.search.as_ref().unwrap().scope, Some(1));
        assert_eq!(app.selected_pos, 1);
        assert_eq!(app.selected_orig(), 40, "lands on first in-column match");

        app.handle_key(key('n'));
        assert_eq!(app.selected_pos, 1, "next match stays in the searched column");
        assert_eq!(app.selected_orig(), 41);
    }

    #[test]
    fn column_search_ignores_other_columns() {
        let mut app = App::new(Dataset::load(&fixture()).unwrap());
        // "item" only appears in `name`, but we search the `id` column (col 0),
        // so there should be no match.
        app.handle_key(key('-'));
        type_str(&mut app, "item");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(app.search.as_ref().unwrap().scope, Some(0));
        assert_eq!(app.status_msg.as_deref(), Some("no match"));
    }

    #[test]
    fn filter_restricts_rows() {
        let mut app = App::new(Dataset::load(&fixture()).unwrap());
        app.handle_key(key('&'));
        type_str(&mut app, "item_004"); // matches item_0040..item_0049
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(app.row_count(), 10);
        assert_eq!(app.orig_row(0), 40);
        // Clearing the filter restores the full view.
        app.handle_key(KeyEvent::from(KeyCode::Esc));
        assert_eq!(app.row_count(), 50);
        assert!(app.filter_query.is_none());
    }

    #[test]
    fn bad_regex_reports_error_without_applying() {
        let mut app = App::new(Dataset::load(&fixture()).unwrap());
        app.handle_key(key('/'));
        type_str(&mut app, "([");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        assert!(app.search.is_none());
        assert!(app.status_msg.as_deref().unwrap().contains("bad pattern"));
    }
}

mod app;
mod browse;
mod data;
mod interrupt;
mod ui;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{self, Event, KeyEventKind};

use app::App;
use data::{Dataset, HeaderSpec, JoinSide, JOIN_MAX_ROWS};

/// A terminal viewer for parquet, CSV/TSV and Excel files, in the manner of
/// csvlens.
#[derive(Parser)]
#[command(name = "lambris", version, about)]
struct Args {
    /// Paths to the data files to view; each one opens in its own tab.
    #[arg(required = true, num_args = 1..)]
    files: Vec<PathBuf>,

    /// Treat the first row as data, not column names (columns become
    /// `column_N`). Toggle it per tab with `T`. Ignored for parquet.
    #[arg(long)]
    no_header: bool,
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
    let mut tabs = Tabs::open(&args.files, header)?;

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
    fn open(paths: &[PathBuf], header: HeaderSpec) -> Result<Self> {
        let mut tabs = Vec::with_capacity(paths.len());
        for path in paths {
            let datasets = Dataset::load_all(path, header)
                .with_context(|| format!("loading {}", path.display()))?;
            tabs.extend(datasets.into_iter().map(|d| vec![App::new(d)]));
        }
        if tabs.is_empty() {
            anyhow::bail!("nothing to show");
        }
        Ok(Self {
            tabs,
            current: 0,
            join: None,
        })
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
        if app.should_quit {
            return false; // quit the whole program from any tab or level
        }
        // Drain the requests first so the tab set can be mutated freely below.
        let exit_transpose = std::mem::take(&mut app.exit_transpose);
        let transpose = std::mem::take(&mut app.transpose_request);
        let switch = app.switch_tab.take();
        let close = std::mem::take(&mut app.close_tab);
        let open = app.open_request.take();
        let toggle_header = std::mem::take(&mut app.toggle_header);
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
            let named = !self.app_mut().data.header.named;
            let skip = self.app_mut().data.header.skip;
            self.reload_header(
                HeaderSpec { skip, named },
                if named { "first row: column names" } else { "first row: data" },
            );
        }
        if promote_header {
            self.promote_header_row();
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
            match Dataset::load_all(&path, header) {
                // A workbook lands as several tabs; the first one becomes current.
                Ok(datasets) => {
                    let first = self.tabs.len();
                    self.tabs
                        .extend(datasets.into_iter().map(|d| vec![App::new(d)]));
                    self.current = first.min(self.tabs.len() - 1);
                }
                Err(e) => self.app_mut().status_msg = Some(format!("open failed: {e}")),
            }
        }
        if close {
            let closed = self.current;
            self.tabs.remove(closed);
            // Tab indices shift, so a pending join pick has to move with them —
            // otherwise it would quietly point at a different table.
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
        if app.data.header.skip > 0 {
            self.reload_header(HeaderSpec::default(), "header: back to the first row");
            return;
        }
        if app.row_count() == 0 {
            return;
        }
        // The raw file row under the cursor, which is what becomes the header.
        let skip = app.data.raw_row(app.selected_orig());
        let spec = HeaderSpec { skip, named: true };
        let msg = format!("header: row {}", spec.header_line());
        self.reload_header(spec, &msg);
    }
}

impl Tabs {
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
        let ((left_tab, left_col), (right_tab, right_col)) = (left, right);
        // A tab could have been closed between the two picks.
        let (Some(left_app), Some(right_app)) = (
            self.tabs.get(left_tab).and_then(|s| s.last()),
            self.tabs.get(right_tab).and_then(|s| s.last()),
        ) else {
            self.app_mut().status_msg = Some("join: that tab is gone".into());
            return;
        };
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
        match joined {
            Ok(Some((dataset, report))) => {
                let mut view = App::new(dataset);
                let mut msg = format!(
                    "{} rows · {} matched, {} unmatched",
                    report.rows, report.matched, report.unmatched
                );
                if report.truncated {
                    msg.push_str(&format!(" · cut at {JOIN_MAX_ROWS}"));
                }
                view.status_msg = Some(msg);
                self.tabs.push(vec![view]);
                self.current = self.tabs.len() - 1;
            }
            Ok(None) => {
                interrupt::take();
                self.app_mut().status_msg = Some("join cancelled".into());
            }
            Err(e) => self.app_mut().status_msg = Some(format!("join failed: {e}")),
        }
    }
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
            Tabs::open(&[root.join("alpha.csv")], HeaderSpec::default()).unwrap();

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
            Tabs::open(&[root.join("alpha.csv")], HeaderSpec::default()).unwrap();
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
            Tabs::open(&[root.join("alpha.csv")], HeaderSpec::default()).unwrap();

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
            Tabs::open(&[root.join("alpha.csv")], HeaderSpec::default()).unwrap();

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
            Tabs::open(&[root.join("alpha.csv")], HeaderSpec::default()).unwrap();
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
            Tabs::open(&[root.join("alpha.csv")], HeaderSpec::default()).unwrap();
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
            Tabs::open(&[write_text_fixture("csv", csv)], HeaderSpec::default()).unwrap();
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
        let mut tabs = Tabs::open(&[meta, dict], HeaderSpec::default()).unwrap();

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
        let mut tabs = Tabs::open(&[left, right], HeaderSpec::default()).unwrap();
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
        let mut tabs = Tabs::open(&[meta, dict], HeaderSpec::default()).unwrap();

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
        let mut tabs = Tabs::open(&[dict, meta], HeaderSpec::default()).unwrap();
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
        let mut tabs = Tabs::open(&[meta, dict], HeaderSpec::default()).unwrap();

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
            Tabs::open(&[spare, meta, dict], HeaderSpec::default()).unwrap();
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
        let mut tabs = Tabs::open(&[left, right], HeaderSpec::default()).unwrap();
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
        // Sections and keys that the one-line hint has no room for.
        for expected in ["Moving", "Finding", "The first row", "Tabs", "keys"] {
            assert!(text.contains(expected), "help missing {expected}: {text}");
        }
        assert!(text.contains("make the selected row the header"));
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
        let mut tabs = Tabs::open(&[write_text_fixture("csv", csv)], HeaderSpec::default())
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
        assert_eq!(tabs.app_mut().data.header.skip, 2);
        assert_eq!(tabs.app_mut().data.header.header_line(), 3);
        // The score column is numeric now that the junk rows are gone.
        assert!(tabs.app_mut().data.is_numeric(2));
        assert!(buffer_text(tabs.app_mut(), 100, 20).contains("header@3"));

        // Pressing H again cancels it, back to the file as it comes.
        tabs.app_mut().handle_key(key('H'));
        assert!(tabs.step());
        assert_eq!(tabs.app_mut().data.header, HeaderSpec::default());
        assert_eq!(tabs.app_mut().data.column_names[0], "exported by hand");
        assert_eq!(tabs.tabs.len(), 1, "it stays one tab throughout");
    }

    #[test]
    fn promoting_counts_from_the_row_under_the_cursor() {
        // Header at the top, so the first data row is raw row 2.
        let csv = "id,name\nalpha,1\nbeta,2\n";
        let mut tabs = Tabs::open(&[write_text_fixture("csv", csv)], HeaderSpec::default())
            .unwrap();
        tabs.app_mut().handle_key(key('H'));
        assert!(tabs.step());
        assert_eq!(tabs.app_mut().data.header.skip, 1, "row under the cursor, not row 0");
        assert_eq!(tabs.app_mut().data.column_names, vec!["alpha", "1"]);
        assert_eq!(tabs.app_mut().row_count(), 1);
    }

    #[test]
    fn promoting_a_row_works_on_a_sheet_and_a_commented_file() {
        // A worksheet: H re-reads just that sheet.
        let mut tabs = Tabs::open(&[xlsx_fixture()], HeaderSpec::default()).unwrap();
        tabs.app_mut().handle_key(key('H'));
        assert!(tabs.step());
        assert_eq!(tabs.app_mut().data.header.skip, 1);
        assert_eq!(tabs.app_mut().data.column_names, vec!["1", "alpha", "3.5"]);
        assert_eq!(tabs.app_mut().row_count(), 3);
        assert_eq!(tabs.tabs.len(), 2, "the other sheet is untouched");

        // A `#` preamble is skipped first, so H counts from the real content.
        let tsv = "# a comment\njunk\tjunk2\nid\tname\n1\talpha\n";
        let mut tabs =
            Tabs::open(&[write_text_fixture("tsv", tsv)], HeaderSpec::default()).unwrap();
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
        assert!(headed.header.named);
        // Without one, every record is data and the names are positional.
        let bare = Dataset::load_all(&path, HeaderSpec::NONE).unwrap().remove(0);
        assert_eq!(bare.column_names, vec!["column_1", "column_2", "column_3"]);
        assert_eq!(bare.nrows, 2);
        assert!(!bare.header.named);
        assert_eq!(bare.cell_display(0, 0).unwrap().as_deref(), Some("1"));
    }

    #[test]
    fn header_toggle_reloads_the_current_tab() {
        let csv = "id,name\n1,alpha\n2,beta\n";
        let mut tabs = Tabs::open(&[write_text_fixture("csv", csv)], HeaderSpec::default()).unwrap();
        assert_eq!(tabs.app_mut().row_count(), 2);

        // `T` re-reads the file with the header row as data.
        tabs.app_mut().handle_key(key('T'));
        assert!(tabs.step());
        assert!(!tabs.app_mut().data.header.named);
        assert_eq!(tabs.app_mut().data.column_names, vec!["column_1", "column_2"]);
        assert_eq!(tabs.app_mut().row_count(), 3, "the header row is now data");
        assert_eq!(tabs.tabs.len(), 1, "toggling stays in the same tab");

        // And back again.
        tabs.app_mut().handle_key(key('T'));
        assert!(tabs.step());
        assert!(tabs.app_mut().data.header.named);
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
        let mut tabs = Tabs::open(&[fixture()], HeaderSpec::default()).unwrap();
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
        assert!(!numbers.header.named);

        // Toggling a workbook tab reloads just that sheet, keeping the others.
        let mut tabs = Tabs::open(&[book], HeaderSpec::default()).unwrap();
        tabs.app_mut().handle_key(code(KeyCode::Tab));
        assert!(tabs.step());
        tabs.app_mut().handle_key(key('T'));
        assert!(tabs.step());
        assert_eq!(tabs.tabs.len(), 2, "the other sheet's tab survives");
        assert_eq!(tabs.current, 1);
        assert!(!tabs.app_mut().data.header.named);
        assert_eq!(tabs.app_mut().data.column_names, vec!["column_1", "column_2"]);
        assert!(tabs.app_mut().data.label.ends_with("[Dates]"));
        // The untouched sheet still has its header.
        tabs.app_mut().handle_key(code(KeyCode::Tab));
        assert!(tabs.step());
        assert!(tabs.app_mut().data.header.named);
    }

    #[test]
    fn xlsx_opens_one_tab_per_sheet() {
        let mut tabs = Tabs::open(&[xlsx_fixture()], HeaderSpec::default()).unwrap();
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
        use std::time::{Duration, Instant};
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
        let mut tabs = Tabs::open(&[fixture(), fixture(), fixture()], HeaderSpec::default()).unwrap();
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
        let mut tabs = Tabs::open(&[fixture(), fixture()], HeaderSpec::default()).unwrap();

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
        let mut tabs = Tabs::open(&[fixture()], HeaderSpec::default()).unwrap();
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
        let mut tabs = Tabs::open(&[fixture()], HeaderSpec::default()).unwrap();
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
        let mut tabs = Tabs::open(&[fixture()], HeaderSpec::default()).unwrap();
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
        let mut tabs = Tabs::open(&paths, HeaderSpec::default()).unwrap();
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
        let mut tabs = Tabs::open(&paths, HeaderSpec::default()).unwrap();
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

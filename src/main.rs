mod app;
mod data;
mod interrupt;
mod ui;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{self, Event, KeyEventKind};

use app::App;
use data::Dataset;

/// A terminal parquet file viewer, in the manner of csvlens.
#[derive(Parser)]
#[command(name = "lambris", version, about)]
struct Args {
    /// Paths to the data files to view; each one opens in its own tab.
    #[arg(required = true, num_args = 1..)]
    files: Vec<PathBuf>,
}

/// Largest number of records turned into columns when transposing, so a huge
/// file can't produce an unbounded number of columns.
const TRANSPOSE_MAX_RECORDS: usize = 4096;

fn main() -> Result<()> {
    let args = Args::parse();
    let mut tabs = Tabs::open(&args.files)?;

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
}

impl Tabs {
    /// Open one tab per path, failing before the TUI starts if any won't load.
    fn open(paths: &[PathBuf]) -> Result<Self> {
        let mut tabs = Vec::with_capacity(paths.len());
        for path in paths {
            let dataset = Dataset::load(path)
                .with_context(|| format!("loading {}", path.display()))?;
            tabs.push(vec![App::new(dataset)]);
        }
        Ok(Self { tabs, current: 0 })
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
        if let Some(delta) = switch {
            let n = self.tabs.len() as isize;
            self.current = (((self.current as isize + delta) % n + n) % n) as usize;
        }
        if let Some(path) = open {
            let path = expand_home(&path);
            match Dataset::load(&path) {
                Ok(dataset) => {
                    self.tabs.push(vec![App::new(dataset)]);
                    self.current = self.tabs.len() - 1;
                }
                Err(e) => self.app_mut().status_msg = Some(format!("open failed: {e}")),
            }
        }
        if close {
            self.tabs.remove(self.current);
            if self.tabs.is_empty() {
                return false; // closed the last tab
            }
            self.current = self.current.min(self.tabs.len() - 1);
        }
        true
    }
}

/// Expand a leading `~/` in a path typed at the `o` prompt.
fn expand_home(input: &str) -> PathBuf {
    match input.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest),
            None => PathBuf::from(input),
        },
        None => PathBuf::from(input),
    }
}

/// Draw the current tab's top view, feed it one key, then let [`Tabs::step`]
/// apply whatever it asked for. Transposed views and freshly opened files run
/// through this very same path, so every command works unchanged.
fn run(terminal: &mut ratatui::DefaultTerminal, tabs: &mut Tabs) -> Result<()> {
    loop {
        let strip = tabs.strip();
        let app = tabs.app_mut();
        terminal
            .draw(|frame| {
                if let Err(e) = ui::render(frame, app, &strip) {
                    app.render_error = Some(e.to_string());
                    app.should_quit = true;
                }
            })
            .context("drawing frame")?;

        if let Event::Key(key) = event::read().context("reading input event")? {
            if key.kind == KeyEventKind::Press {
                app.handle_key(key);
            }
        }

        if let Some(e) = app.render_error.take() {
            anyhow::bail!("render error: {e}");
        }
        if !tabs.step() {
            break;
        }
    }
    Ok(())
}

/// Build a transposed view from the current app's view, capped in width.
fn transposed_view(app: &App) -> Result<App> {
    if app.data.ncols < 2 || app.row_count() == 0 {
        anyhow::bail!("needs at least 2 columns and 1 row");
    }
    let rows = app.view_rows(TRANSPOSE_MAX_RECORDS);
    let dataset = app.data.transpose(&rows)?;
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

    fn buffer_text(app: &mut App, w: u16, h: u16) -> String {
        buffer_text_tabs(app, &ui::TabStrip::default(), w, h)
    }

    /// Render `app` as the visible view of `tabs` (an empty strip = one tab).
    fn buffer_text_tabs(app: &mut App, tabs: &ui::TabStrip, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal
            .draw(|f| ui::render(f, app, tabs).unwrap())
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
            .filter_rows(&regex::Regex::new("^18000$").unwrap(), || false)
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
            .filter_rows(&regex::Regex::new("x").unwrap(), always)
            .unwrap()
            .is_none());

        // Search for a value that exists near the end: found normally, but a
        // firing cancel aborts before reaching it.
        let re = regex::Regex::new("^19999$").unwrap();
        assert_eq!(
            ds.find_match(&re, BIG as usize, |i| i, 0, 0, true, Some(0), never),
            Some((19999, 0)),
        );
        assert!(ds
            .find_match(&re, BIG as usize, |i| i, 0, 0, true, Some(0), always)
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
        assert_eq!(app.selected_col, 0);
        app.handle_key(KeyEvent::from(KeyCode::Char('$')));
        assert_eq!(app.selected_col, 2);
        app.handle_key(KeyEvent::from(KeyCode::Right)); // past last col
        assert_eq!(app.selected_col, 2);
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
        let t = ds.transpose(&[0, 1]).unwrap();

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
        assert_eq!(tv.selected_col, 1);
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
        let mut tabs = Tabs::open(&[fixture(), fixture(), fixture()]).unwrap();
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
        let mut tabs = Tabs::open(&[fixture(), fixture()]).unwrap();

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
        let mut tabs = Tabs::open(&[fixture()]).unwrap();
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
        let mut tabs = Tabs::open(&[fixture()]).unwrap();
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
        let mut tabs = Tabs::open(&[fixture()]).unwrap();
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
        let mut tabs = Tabs::open(&paths).unwrap();
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
        let mut tabs = Tabs::open(&paths).unwrap();
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
        assert_eq!(app.selected_col, 2);

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
        assert_eq!(app.selected_col, 1);
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
        assert_eq!(app.selected_col, 1, "should land on the name column");
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
        assert_eq!(app.selected_col, 1);
        assert_eq!(app.selected_orig(), 40, "lands on first in-column match");

        app.handle_key(key('n'));
        assert_eq!(app.selected_col, 1, "next match stays in the searched column");
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

mod app;
mod data;
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
    /// Path to the parquet file to view.
    file: PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let dataset = Dataset::load(&args.file)?;
    let mut app = App::new(dataset);

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut app);
    ratatui::restore();
    result
}

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    while !app.should_quit {
        terminal
            .draw(|frame| {
                // Rendering only fails on formatter construction, which is a
                // hard error worth surfacing after we restore the terminal.
                if let Err(e) = ui::render(frame, app) {
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
    }
    if let Some(e) = app.render_error.take() {
        anyhow::bail!("render error: {e}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use arrow::array::{Int64Array, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use crossterm::event::{KeyCode, KeyEvent};
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

        let mut path = std::env::temp_dir();
        path.push("lambris_test_fixture.parquet");
        let file = std::fs::File::create(&path).unwrap();
        let mut writer = ArrowWriter::try_new(file, schema, None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        path
    }

    fn buffer_text(app: &mut App, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal
            .draw(|f| ui::render(f, app).unwrap())
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
        assert!(text.contains("i info"), "command hints missing: {text}");
        assert!(!text.contains("Int64"), "type shown outside info mode: {text}");

        // Pressing `i` reveals the selected column's type and value.
        app.handle_key(key('i'));
        assert!(app.show_info);
        let text = buffer_text(&mut app, 100, 20);
        assert!(text.contains("id: Int64"), "info line missing type: {text}");
        assert!(!text.contains("i info"), "hints should be replaced by info: {text}");

        // `i` again toggles back to the hints.
        app.handle_key(key('i'));
        assert!(!app.show_info);
    }

    #[test]
    fn goto_line_jumps_to_row() {
        let mut app = App::new(Dataset::load(&fixture()).unwrap());
        app.handle_key(key(':'));
        type_str(&mut app, "27");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(app.rows[app.selected_row], 26, "line 27 is original row 26");

        // Non-digits are rejected at the prompt, so this stays empty and no-ops.
        app.handle_key(key(':'));
        type_str(&mut app, "abc");
        assert_eq!(app.input, "");
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(app.rows[app.selected_row], 26, "selection unchanged");
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
        assert_eq!(app.rows[app.selected_row], 42);
        assert_eq!(app.selected_col, 1, "should land on the name column");
        assert!(app.search.is_some());
    }

    #[test]
    fn filter_restricts_rows() {
        let mut app = App::new(Dataset::load(&fixture()).unwrap());
        app.handle_key(key('&'));
        type_str(&mut app, "item_004"); // matches item_0040..item_0049
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(app.row_count(), 10);
        assert_eq!(app.rows[0], 40);
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

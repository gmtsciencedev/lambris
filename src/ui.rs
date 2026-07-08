use std::collections::HashMap;

use anyhow::Result;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table};
use ratatui::Frame;

use crate::app::{App, InputKind, Mode};

const MAX_COL_WIDTH: u16 = 40;
const MIN_COL_WIDTH: u16 = 3;
const COL_SPACING: u16 = 1;
const NA: &str = "NA";

/// Formatted cells for one column over the visible row window (`None` = null),
/// plus the display width they imply.
struct RenderedColumn {
    width: u16,
    cells: Vec<Option<String>>,
}

pub fn render(frame: &mut Frame, app: &mut App) -> Result<()> {
    let [title_area, body_area, status_area, help_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    // Body has one header row; the rest is scrollable data.
    let viewport_rows = body_area.height.saturating_sub(1) as usize;
    app.viewport_rows = viewport_rows.max(1);

    render_title(frame, title_area, app);
    render_status(frame, status_area, app);
    render_help(frame, help_area, app);

    if app.row_count() == 0 {
        let msg = Paragraph::new("— no rows match the filter —")
            .style(Style::new().fg(Color::Yellow))
            .alignment(Alignment::Center);
        frame.render_widget(msg, body_area);
        return Ok(());
    }

    // Keep the selected row inside the vertical viewport.
    if app.selected_row < app.row_offset {
        app.row_offset = app.selected_row;
    } else if app.selected_row >= app.row_offset + app.viewport_rows {
        app.row_offset = app.selected_row + 1 - app.viewport_rows;
    }
    let row_start = app.row_offset;
    let row_end = (row_start + app.viewport_rows).min(app.row_count());
    let visible_view: Vec<usize> = (row_start..row_end).collect();

    render_table(frame, body_area, app, &visible_view)?;
    Ok(())
}

fn render_title(frame: &mut Frame, area: Rect, app: &App) {
    let name = app
        .data
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| app.data.path.display().to_string());
    let line = Line::from(vec![
        Span::styled(
            format!(" {name} "),
            Style::new().bold().bg(Color::Blue).fg(Color::White),
        ),
        Span::raw(format!("  {} rows × {} cols", app.data.nrows, app.data.ncols)),
    ]);
    frame.render_widget(line, area);
}

fn render_table(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    visible_view: &[usize],
) -> Result<()> {
    // Original dataset row indices behind the visible view rows.
    let visible_orig: Vec<usize> = visible_view.iter().map(|&v| app.rows[v]).collect();

    let gutter = gutter_width(app);
    let available = area.width.saturating_sub(gutter + COL_SPACING);

    let mut cache: HashMap<usize, RenderedColumn> = HashMap::new();
    let visible_cols = fit_columns(app, &visible_orig, available, &mut cache)?;

    // Header row: gutter label + selected-aware column names.
    let mut header_cells = vec![Cell::from("#").style(Style::new().dim())];
    for &col in &visible_cols {
        let mut style = Style::new().bold().fg(Color::Cyan);
        if col == app.selected_col {
            style = style.add_modifier(Modifier::REVERSED);
        }
        let name = truncate(&app.data.column_names[col], cache[&col].width);
        header_cells.push(Cell::from(name).style(style));
    }
    let header = Row::new(header_cells).style(Style::new().underlined());

    // Body rows.
    let search_re = app.search.as_ref().map(|s| &s.re);
    let mut rows = Vec::with_capacity(visible_view.len());
    for (i, &vi) in visible_view.iter().enumerate() {
        let orig = app.rows[vi];
        let sel_row = vi == app.selected_row;
        let mut cells = vec![Cell::from(format!("{}", orig + 1)).style(Style::new().dim())];
        for &col in &visible_cols {
            let width = cache[&col].width;
            let sel_cell = sel_row && col == app.selected_col;
            let (text, mut style) = match &cache[&col].cells[i] {
                None => (
                    NA.to_string(),
                    Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Some(s) => {
                    let is_match =
                        search_re.map(|re| re.is_match(s)).unwrap_or(false);
                    let base = if is_match {
                        Style::new().bg(Color::Yellow).fg(Color::Black)
                    } else {
                        Style::new()
                    };
                    (truncate(s, width), base)
                }
            };
            if sel_row && !sel_cell {
                style = style.bg(Color::Rgb(40, 40, 55));
            }
            if sel_cell {
                style = style.add_modifier(Modifier::REVERSED);
            }
            cells.push(Cell::from(text).style(style));
        }
        rows.push(Row::new(cells));
    }

    let mut widths = vec![Constraint::Length(gutter)];
    widths.extend(visible_cols.iter().map(|c| Constraint::Length(cache[c].width)));

    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(COL_SPACING);
    frame.render_widget(table, area);
    Ok(())
}

/// Decide which columns fit starting at `col_offset`, scrolling right if
/// needed so the selected column stays visible. Populates `cache`.
fn fit_columns(
    app: &mut App,
    visible_orig: &[usize],
    available: u16,
    cache: &mut HashMap<usize, RenderedColumn>,
) -> Result<Vec<usize>> {
    if app.selected_col < app.col_offset {
        app.col_offset = app.selected_col;
    }
    loop {
        let mut cols = Vec::new();
        let mut used = 0u16;
        let mut c = app.col_offset;
        while c < app.data.ncols {
            render_column(app, c, visible_orig, cache)?;
            let needed = cache[&c].width + COL_SPACING;
            if !cols.is_empty() && used + needed > available {
                break;
            }
            used += needed;
            cols.push(c);
            c += 1;
        }
        let last_visible = *cols.last().unwrap_or(&app.col_offset);
        if app.selected_col <= last_visible {
            return Ok(cols);
        }
        app.col_offset += 1;
    }
}

fn render_column(
    app: &App,
    col: usize,
    visible_orig: &[usize],
    cache: &mut HashMap<usize, RenderedColumn>,
) -> Result<()> {
    if cache.contains_key(&col) {
        return Ok(());
    }
    let formatter = &app.data.formatters(&[col])?[0];
    let mut width = app.data.column_names[col].chars().count() as u16;
    let mut cells = Vec::with_capacity(visible_orig.len());
    for &r in visible_orig {
        if app.data.is_null(col, r) {
            width = width.max(NA.len() as u16);
            cells.push(None);
        } else {
            let s = formatter.value(r).to_string();
            width = width.max(s.chars().count() as u16);
            cells.push(Some(s));
        }
    }
    let width = width.clamp(MIN_COL_WIDTH, MAX_COL_WIDTH);
    cache.insert(col, RenderedColumn { width, cells });
    Ok(())
}

fn gutter_width(app: &App) -> u16 {
    let max_row = app.data.nrows.max(1);
    (max_row.to_string().len() as u16).max(2)
}

fn render_status(frame: &mut Frame, area: Rect, app: &App) {
    // In input mode the status bar becomes the search/filter prompt.
    if let Mode::Input(kind) = app.mode {
        let sigil = match kind {
            InputKind::Search => '/',
            InputKind::Filter => '&',
        };
        let line = Line::from(vec![
            Span::styled(
                format!("{sigil}{}", app.input),
                Style::new().fg(Color::White),
            ),
            Span::styled("▏", Style::new().fg(Color::White).add_modifier(Modifier::SLOW_BLINK)),
        ]);
        frame.render_widget(line, area);
        return;
    }

    let (sel_row, count) = if app.row_count() == 0 {
        (0, 0)
    } else {
        (app.selected_row + 1, app.row_count())
    };

    let mut spans = vec![Span::styled(
        format!(" row {sel_row}/{count}  col {}/{} ", app.selected_col + 1, app.data.ncols),
        Style::new().bg(Color::DarkGray).fg(Color::White),
    )];
    if let Some(search) = &app.search {
        spans.push(Span::styled(
            format!("  /{}", search.query),
            Style::new().fg(Color::Cyan),
        ));
    }
    if let Some(q) = &app.filter_query {
        spans.push(Span::styled(
            format!("  &{q} ({}/{})", app.row_count(), app.data.nrows),
            Style::new().fg(Color::Green),
        ));
    }
    if let Some(msg) = &app.status_msg {
        spans.push(Span::styled(format!("  {msg}"), Style::new().fg(Color::Magenta)));
    }
    frame.render_widget(Line::from(spans), area);
}

/// The bottom line: command hints normally, the input legend while typing, or
/// the selected column's info when info mode (`i`) is on.
fn render_help(frame: &mut Frame, area: Rect, app: &App) {
    let line = match app.mode {
        Mode::Input(_) => Line::from(Span::styled(
            " Enter: apply · Esc: cancel",
            Style::new().dim(),
        )),
        Mode::Normal if app.show_info => info_line(app, area.width),
        Mode::Normal => Line::from(Span::styled(
            " j/k/h/l move · g/G top/bot · / search · n/N next · & filter · i info · q quit",
            Style::new().dim(),
        )),
    };
    frame.render_widget(line, area);
}

/// Describe the selected column and the full value of the selected cell.
fn info_line(app: &App, width: u16) -> Line<'static> {
    if app.data.ncols == 0 || app.row_count() == 0 {
        return Line::from(Span::styled(" (no data)", Style::new().dim()));
    }
    let col = app.selected_col;
    let name = &app.data.column_names[col];
    let ty = &app.data.column_types[col];
    let orig = app.rows[app.selected_row];
    let value = match app.data.cell_display(col, orig) {
        Ok(Some(v)) => v,
        Ok(None) => "NA".to_string(),
        Err(_) => "<error>".to_string(),
    };
    let head = format!(" {name}: {ty}  = ");
    // Keep the whole line within the terminal width.
    let budget = (width as usize).saturating_sub(head.chars().count() + 1);
    let value = truncate(&value, budget as u16);
    Line::from(vec![
        Span::styled(head, Style::new().fg(Color::Yellow).bold()),
        Span::styled(value, Style::new().fg(Color::White)),
    ])
}

/// Clip `s` to `width` display columns, adding an ellipsis when truncated.
fn truncate(s: &str, width: u16) -> String {
    let width = width as usize;
    if s.chars().count() <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let take = width.saturating_sub(1);
    let mut out: String = s.chars().take(take).collect();
    out.push('…');
    out
}

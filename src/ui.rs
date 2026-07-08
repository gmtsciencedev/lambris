use std::collections::HashMap;

use anyhow::Result;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Row, Table};
use ratatui::Frame;

use crate::app::App;

const MAX_COL_WIDTH: u16 = 40;
const MIN_COL_WIDTH: u16 = 3;
const COL_SPACING: u16 = 1;

/// Formatted strings for one column over the visible row window, plus the
/// display width they imply.
struct RenderedColumn {
    width: u16,
    cells: Vec<String>,
}

pub fn render(frame: &mut Frame, app: &mut App) -> Result<()> {
    let [title_area, body_area, status_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    // Body has one header row; the rest is scrollable data.
    let viewport_rows = body_area.height.saturating_sub(1) as usize;
    app.viewport_rows = viewport_rows.max(1);

    // Keep the selected row inside the vertical viewport.
    if app.selected_row < app.row_offset {
        app.row_offset = app.selected_row;
    } else if app.selected_row >= app.row_offset + app.viewport_rows {
        app.row_offset = app.selected_row + 1 - app.viewport_rows;
    }
    let row_start = app.row_offset;
    let row_end = (row_start + app.viewport_rows).min(app.data.nrows);
    let visible_rows: Vec<usize> = (row_start..row_end).collect();

    render_title(frame, title_area, app);
    render_table(frame, body_area, app, &visible_rows)?;
    render_status(frame, status_area, app);
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
        Span::styled(format!(" {name} "), Style::new().bold().bg(Color::Blue).fg(Color::White)),
        Span::raw(format!(
            "  {} rows × {} cols",
            app.data.nrows, app.data.ncols
        )),
    ]);
    frame.render_widget(line, area);
}

fn render_table(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    visible_rows: &[usize],
) -> Result<()> {
    let gutter = gutter_width(app);
    let available = area.width.saturating_sub(gutter + COL_SPACING);

    let mut cache: HashMap<usize, RenderedColumn> = HashMap::new();
    let visible_cols = fit_columns(app, visible_rows, available, &mut cache)?;

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
    let mut rows = Vec::with_capacity(visible_rows.len());
    for (vi, &r) in visible_rows.iter().enumerate() {
        let mut cells = vec![Cell::from(format!("{}", r + 1)).style(Style::new().dim())];
        for &col in &visible_cols {
            let text = truncate(&cache[&col].cells[vi], cache[&col].width);
            let mut style = Style::new();
            if r == app.selected_row && col == app.selected_col {
                style = style.add_modifier(Modifier::REVERSED);
            } else if r == app.selected_row {
                style = style.bg(Color::Rgb(40, 40, 55));
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
    visible_rows: &[usize],
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
            render_column(app, c, visible_rows, cache)?;
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
    visible_rows: &[usize],
    cache: &mut HashMap<usize, RenderedColumn>,
) -> Result<()> {
    if cache.contains_key(&col) {
        return Ok(());
    }
    let formatter = &app.data.formatters(&[col])?[0];
    let mut width = app.data.column_names[col].chars().count() as u16;
    let mut cells = Vec::with_capacity(visible_rows.len());
    for &r in visible_rows {
        let s = formatter.value(r).to_string();
        width = width.max(s.chars().count() as u16);
        cells.push(s);
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
    let (name, ty) = if app.data.ncols > 0 {
        (
            app.data.column_names[app.selected_col].as_str(),
            app.data.column_types[app.selected_col].as_str(),
        )
    } else {
        ("", "")
    };
    let pos = format!(
        " row {}/{}  col {}/{} ",
        app.selected_row + 1,
        app.data.nrows,
        app.selected_col + 1,
        app.data.ncols,
    );
    let hints = "  j/k/h/l move · g/G top/bottom · ^f/^b page · 0/$ col ends · q quit";
    let line = Line::from(vec![
        Span::styled(pos, Style::new().bg(Color::DarkGray).fg(Color::White)),
        Span::styled(format!("  {name}: {ty}"), Style::new().fg(Color::Yellow)),
        Span::styled(hints, Style::new().dim()),
    ]);
    frame.render_widget(line, area);
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

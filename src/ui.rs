use std::collections::HashMap;

use anyhow::Result;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table};
use ratatui::Frame;

use crate::app::{App, InputKind, Mode, NumStyle, SortDir};

const MAX_COL_WIDTH: u16 = 40;
const MIN_COL_WIDTH: u16 = 3;
const COL_SPACING: u16 = 1;
const NA: &str = "NA";

/// Formatted cells for one column over the visible row window (`None` = null),
/// plus the display width they imply and, for log-coloured numeric columns, a
/// per-cell foreground colour.
struct RenderedColumn {
    width: u16,
    cells: Vec<Option<String>>,
    colors: Option<Vec<Option<Color>>>,
}

pub fn render(frame: &mut Frame, app: &mut App) -> Result<()> {
    let [title_area, body_area, status_area, help_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    if app.transpose {
        render_title(frame, title_area, app);
        render_transpose(frame, body_area, app)?;
        render_transpose_status(frame, status_area, app);
        frame.render_widget(
            Line::from(Span::styled(
                " j/k field · h/l record · s sort · %/</> numeric · t exit · q quit",
                Style::new().dim(),
            )),
            help_area,
        );
        return Ok(());
    }

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
    let visible_orig: Vec<usize> = visible_view.iter().map(|&v| app.orig_row(v)).collect();

    let gutter = if app.show_line_numbers {
        gutter_width(app)
    } else {
        0
    };
    let available = if app.show_line_numbers {
        area.width.saturating_sub(gutter + COL_SPACING)
    } else {
        area.width
    };

    let mut cache: HashMap<usize, RenderedColumn> = HashMap::new();
    let (visible_cols, frozen) = fit_columns(app, &visible_orig, available, &mut cache)?;
    // Draw a divider only when frozen columns sit beside a scrollable region.
    let divider_at = (frozen > 0 && visible_cols.len() > frozen).then_some(frozen);

    let frozen_bg = Color::Rgb(30, 30, 45);
    let sel_bg = Color::Rgb(40, 40, 55);

    // Header row: gutter label + selected-aware column names with sort arrows.
    let mut header_cells = Vec::new();
    if app.show_line_numbers {
        header_cells.push(Cell::from("#").style(Style::new().dim()));
    }
    for (idx, &col) in visible_cols.iter().enumerate() {
        if divider_at == Some(idx) {
            header_cells.push(divider_cell());
        }
        let arrow = match app.sort {
            Some(s) if s.col == col && s.dir == SortDir::Asc => " ↑",
            Some(s) if s.col == col && s.dir == SortDir::Desc => " ↓",
            _ => "",
        };
        let label = truncate(
            &format!("{}{arrow}", app.data.column_names[col]),
            cache[&col].width,
        );
        let base = if idx < frozen { Color::Magenta } else { Color::Cyan };
        let mut style = Style::new().bold().fg(base);
        if col == app.selected_col {
            style = style.add_modifier(Modifier::REVERSED);
        }
        header_cells.push(Cell::from(label).style(style));
    }
    let header = Row::new(header_cells).style(Style::new().underlined());

    // Body rows.
    let search = app.search.as_ref();
    let mut rows = Vec::with_capacity(visible_view.len());
    for (i, &vi) in visible_view.iter().enumerate() {
        let orig = app.orig_row(vi);
        let sel_row = vi == app.selected_row;
        let mut cells = Vec::new();
        if app.show_line_numbers {
            cells.push(Cell::from(format!("{}", orig + 1)).style(Style::new().dim()));
        }
        for (idx, &col) in visible_cols.iter().enumerate() {
            if divider_at == Some(idx) {
                cells.push(divider_cell());
            }
            let width = cache[&col].width;
            let sel_cell = sel_row && col == app.selected_col;
            let (text, mut style) = match &cache[&col].cells[i] {
                None => (
                    NA.to_string(),
                    Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Some(s) => {
                    // Only highlight in-scope columns for a column search.
                    let is_match = search
                        .map(|se| {
                            se.scope.map(|sc| sc == col).unwrap_or(true) && se.re.is_match(s)
                        })
                        .unwrap_or(false);
                    let base = if is_match {
                        Style::new().bg(Color::Yellow).fg(Color::Black)
                    } else if let Some(color) = cache[&col]
                        .colors
                        .as_ref()
                        .and_then(|cs| cs.get(i).copied().flatten())
                    {
                        Style::new().fg(color)
                    } else {
                        Style::new()
                    };
                    (truncate(s, width), base)
                }
            };
            // Background priority: selected row > frozen tint.
            if sel_row && !sel_cell {
                style = style.bg(sel_bg);
            } else if idx < frozen && !sel_cell {
                style = style.bg(frozen_bg);
            }
            if sel_cell {
                style = style.add_modifier(Modifier::REVERSED);
            }
            cells.push(Cell::from(text).style(style));
        }
        rows.push(Row::new(cells));
    }

    let mut widths = Vec::new();
    if app.show_line_numbers {
        widths.push(Constraint::Length(gutter));
    }
    for (idx, &col) in visible_cols.iter().enumerate() {
        if divider_at == Some(idx) {
            widths.push(Constraint::Length(1));
        }
        widths.push(Constraint::Length(cache[&col].width));
    }

    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(COL_SPACING);
    frame.render_widget(table, area);
    Ok(())
}

fn divider_cell() -> Cell<'static> {
    Cell::from("│").style(Style::new().fg(Color::DarkGray))
}

const NAME_COL_MAX: u16 = 30;

/// The column whose values title the records in the transposed view.
const TITLE_COL: usize = 0;

/// Render the transposed view: original columns run down the left as a field
/// column, and a horizontally-scrollable window of records runs across the top,
/// titled by the first column's values. Only the on-screen records are read, so
/// this stays cheap on big files.
fn render_transpose(frame: &mut Frame, area: Rect, app: &mut App) -> Result<()> {
    let data = &app.data;
    let ncols = data.ncols;
    let row_count = app.row_count();

    // One header row (record titles); the rest lists fields (columns 1..ncols).
    let field_rows = area.height.saturating_sub(1).max(1) as usize;

    // Vertical scroll over fields, which start at column 1 (0 is the title).
    let mut field_off = app.t_field_offset.max(1);
    if app.t_field < field_off {
        field_off = app.t_field;
    } else if app.t_field >= field_off + field_rows {
        field_off = app.t_field + 1 - field_rows;
    }
    field_off = field_off.max(1);
    let field_end = (field_off + field_rows).min(ncols);
    let fields: Vec<usize> = (field_off..field_end).collect();

    // The name column must also fit the title column's name (the corner label).
    let name_w = std::iter::once(data.column_names[TITLE_COL].chars().count())
        .chain(fields.iter().map(|&c| data.column_names[c].chars().count()))
        .max()
        .unwrap_or(MIN_COL_WIDTH as usize)
        .clamp(MIN_COL_WIDTH as usize, NAME_COL_MAX as usize) as u16;

    // Cache cell strings fetched while measuring so rendering reuses them.
    let mut cache: HashMap<(usize, usize), Option<String>> = HashMap::new();
    let mut cell = |c: usize, orig: usize, data: &crate::data::Dataset| -> Option<String> {
        cache
            .entry((c, orig))
            .or_insert_with(|| data.cell_display(c, orig).ok().flatten())
            .clone()
    };
    let title_of = |c: Option<String>| c.unwrap_or_else(|| NA.to_string());

    // Horizontal fit: which records fit across, scrolling to keep t_record in view.
    let mut record_off = app.t_record_offset.min(app.t_record);
    let avail = area.width;
    let (records, widths): (Vec<usize>, Vec<u16>) = loop {
        let mut recs = Vec::new();
        let mut ws = Vec::new();
        let mut used = name_w + COL_SPACING;
        let mut r = record_off;
        while r < row_count {
            let orig = app.orig_row(r);
            // Column width covers the title and every visible field value.
            let mut w = title_of(cell(TITLE_COL, orig, data)).chars().count() as u16;
            for &c in &fields {
                let (disp, _) = style_value(cell(c, orig, data), app.num_styles.get(&c).copied());
                let vw = match disp {
                    Some(s) => s.chars().count() as u16,
                    None => NA.len() as u16,
                };
                w = w.max(vw);
            }
            let w = w.clamp(MIN_COL_WIDTH, MAX_COL_WIDTH);
            if !recs.is_empty() && used + w + COL_SPACING > avail {
                break;
            }
            used += w + COL_SPACING;
            recs.push(r);
            ws.push(w);
            r += 1;
        }
        if app.t_record <= *recs.last().unwrap_or(&record_off) {
            break (recs, ws);
        }
        record_off += 1;
    };

    let sel_bg = Color::Rgb(40, 40, 55);

    // Header: the title column's name in the corner, then each record's title.
    let mut header_cells = vec![
        Cell::from(truncate(&data.column_names[TITLE_COL], name_w))
            .style(Style::new().bold().fg(Color::Magenta)),
    ];
    for (j, &r) in records.iter().enumerate() {
        let orig = app.orig_row(r);
        let title = title_of(cell(TITLE_COL, orig, data));
        let mut style = Style::new().bold().fg(Color::Cyan);
        if r == app.t_record {
            style = style.add_modifier(Modifier::REVERSED);
        }
        header_cells.push(Cell::from(truncate(&title, widths[j])).style(style));
    }
    let header = Row::new(header_cells).style(Style::new().underlined());

    let mut rows = Vec::with_capacity(fields.len());
    for &c in &fields {
        let sel_field = c == app.t_field;
        let name_style = {
            let mut s = Style::new().bold().fg(Color::Cyan);
            if sel_field {
                s = s.add_modifier(Modifier::REVERSED);
            }
            s
        };
        let mut cells = vec![Cell::from(truncate(&data.column_names[c], name_w)).style(name_style)];
        for (j, &r) in records.iter().enumerate() {
            let orig = app.orig_row(r);
            let sel_cell = sel_field && r == app.t_record;
            let (disp, color) = style_value(cell(c, orig, data), app.num_styles.get(&c).copied());
            let (text, mut style) = match disp {
                None => (
                    NA.to_string(),
                    Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                Some(s) => {
                    let base = match color {
                        Some(col) => Style::new().fg(col),
                        None => Style::new(),
                    };
                    (truncate(&s, widths[j]), base)
                }
            };
            if !sel_cell && (sel_field || r == app.t_record) {
                style = style.bg(sel_bg);
            }
            if sel_cell {
                style = style.add_modifier(Modifier::REVERSED);
            }
            cells.push(Cell::from(text).style(style));
        }
        rows.push(Row::new(cells));
    }

    let mut constraints = vec![Constraint::Length(name_w)];
    constraints.extend(widths.iter().map(|&w| Constraint::Length(w)));
    let table = Table::new(rows, constraints)
        .header(header)
        .column_spacing(COL_SPACING);
    frame.render_widget(table, area);

    app.t_field_offset = field_off;
    app.t_record_offset = record_off;
    app.t_field_page = field_rows;
    Ok(())
}

fn render_transpose_status(frame: &mut Frame, area: Rect, app: &App) {
    let name = app
        .data
        .column_names
        .get(app.t_field)
        .map(String::as_str)
        .unwrap_or("");
    let ty = app
        .data
        .column_types
        .get(app.t_field)
        .map(String::as_str)
        .unwrap_or("");
    // Fields are columns 1..ncols (column 0 titles the records).
    let mut spans = vec![
        Span::styled(
            format!(
                " ⇄ transpose  field {}/{}  record {}/{} ",
                app.t_field,
                app.data.ncols.saturating_sub(1),
                app.t_record + 1,
                app.row_count(),
            ),
            Style::new().bg(Color::DarkGray).fg(Color::White),
        ),
        Span::styled(
            format!("  {} · {name}: {ty}", app.data.column_names[TITLE_COL]),
            Style::new().fg(Color::Yellow),
        ),
    ];
    if let Some(s) = app.sort {
        let arrow = if s.dir == SortDir::Asc { "↑" } else { "↓" };
        spans.push(Span::styled(
            format!("  sort {arrow}{}", app.data.column_names[s.col]),
            Style::new().fg(Color::Blue),
        ));
    }
    if let Some(st) = app.num_styles.get(&app.t_field) {
        let dec = st.decimals.map(|n| format!(".{n}")).unwrap_or_default();
        let log = if st.log { " log" } else { "" };
        spans.push(Span::styled(
            format!("  num{dec}{log}"),
            Style::new().fg(Color::Green),
        ));
    }
    frame.render_widget(Line::from(spans), area);
}

/// Decide which columns fit: the frozen prefix `0..frozen` (always shown),
/// then a scrollable region from `col_offset`, scrolling right so the selected
/// column stays visible. Returns the visible columns and the frozen count.
fn fit_columns(
    app: &mut App,
    visible_orig: &[usize],
    available: u16,
    cache: &mut HashMap<usize, RenderedColumn>,
) -> Result<(Vec<usize>, usize)> {
    let frozen = app.frozen_cols.min(app.data.ncols);

    // Frozen columns are always present and consume width up front.
    let mut frozen_cols = Vec::with_capacity(frozen);
    let mut frozen_width = 0u16;
    for c in 0..frozen {
        render_column(app, c, visible_orig, cache)?;
        frozen_width += cache[&c].width + COL_SPACING;
        frozen_cols.push(c);
    }
    // The divider between frozen and scrollable regions costs one column.
    if frozen > 0 {
        frozen_width += 1 + COL_SPACING;
    }

    // The scrollable region never starts before the frozen prefix.
    if app.col_offset < frozen {
        app.col_offset = frozen;
    }
    if app.selected_col >= frozen && app.selected_col < app.col_offset {
        app.col_offset = app.selected_col;
    }

    loop {
        let mut scroll = Vec::new();
        let mut used = frozen_width;
        let mut c = app.col_offset;
        while c < app.data.ncols {
            render_column(app, c, visible_orig, cache)?;
            let needed = cache[&c].width + COL_SPACING;
            if !scroll.is_empty() && used + needed > available {
                break;
            }
            used += needed;
            scroll.push(c);
            c += 1;
        }
        let last_visible = *scroll.last().unwrap_or(&app.col_offset);
        // Selected column is either frozen (always visible) or within the scroll.
        if app.selected_col < frozen || app.selected_col <= last_visible {
            let mut all = frozen_cols;
            all.extend(scroll);
            return Ok((all, frozen));
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
    let mut width = app.data.column_names[col].chars().count() as u16;
    // Reserve room for the sort arrow shown next to a sorted column's header.
    if app.sort.map(|s| s.col == col).unwrap_or(false) {
        width += 2;
    }
    let raw = app.data.cells(col, visible_orig)?;
    let (cells, colors) = match app.num_styles.get(&col) {
        Some(st) if st.align => build_numeric(&raw, *st),
        _ => (raw, None),
    };
    for cell in &cells {
        let cell_width = match cell {
            Some(s) => s.chars().count() as u16,
            None => NA.len() as u16,
        };
        width = width.max(cell_width);
    }
    let width = width.clamp(MIN_COL_WIDTH, MAX_COL_WIDTH);
    cache.insert(
        col,
        RenderedColumn {
            width,
            cells,
            colors,
        },
    );
    Ok(())
}

/// Reformat numeric cells: apply fixed decimals if set, pad so decimal points
/// line up, and (when `log`) compute a per-cell colour from the value's
/// magnitude. Non-numeric or null cells are passed through untouched.
fn build_numeric(
    raw: &[Option<String>],
    st: NumStyle,
) -> (Vec<Option<String>>, Option<Vec<Option<Color>>>) {
    let values: Vec<Option<f64>> = raw
        .iter()
        .map(|c| c.as_ref().and_then(|s| s.trim().parse::<f64>().ok()))
        .collect();

    // Text form of each numeric cell (fixed decimals, or natural), split into
    // integer and fractional parts for alignment.
    let mut max_int = 0usize;
    let mut max_frac = 0usize;
    let parts: Vec<Option<(String, String)>> = raw
        .iter()
        .zip(&values)
        .map(|(cell, value)| {
            let (cell, value) = (cell.as_ref()?, (*value)?);
            let text = match st.decimals {
                Some(n) => format!("{value:.*}", n as usize),
                None => cell.trim().to_string(),
            };
            let (int, frac) = match text.split_once('.') {
                Some((i, f)) => (i.to_string(), f.to_string()),
                None => (text, String::new()),
            };
            max_int = max_int.max(int.chars().count());
            max_frac = max_frac.max(frac.chars().count());
            Some((int, frac))
        })
        .collect();

    let cells: Vec<Option<String>> = raw
        .iter()
        .zip(&parts)
        .map(|(orig, part)| match part {
            Some((int, frac)) => Some(if max_frac > 0 {
                let dot = if frac.is_empty() { ' ' } else { '.' };
                format!("{int:>max_int$}{dot}{frac:<max_frac$}")
            } else {
                format!("{int:>max_int$}")
            }),
            // Null or unparseable: leave the original string (or None) as-is.
            None => orig.clone(),
        })
        .collect();

    let colors = st
        .log
        .then(|| values.iter().map(|v| v.map(log_color)).collect());
    (cells, colors)
}

/// Apply a numeric style to a single value without decimal-point alignment
/// (used by the transposed view, where a field's values are laid out
/// horizontally). Returns the display text and an optional log colour.
fn style_value(raw: Option<String>, st: Option<NumStyle>) -> (Option<String>, Option<Color>) {
    match (raw, st) {
        (Some(s), Some(st)) if st.decimals.is_some() || st.log => {
            let value = s.trim().parse::<f64>().ok();
            let text = match (st.decimals, value) {
                (Some(n), Some(v)) => format!("{v:.*}", n as usize),
                _ => s,
            };
            let color = st.log.then(|| value.map(log_color)).flatten();
            (Some(text), color)
        }
        (raw, _) => (raw, None),
    }
}

/// Colour a value by the base-10 log of its magnitude, cool (small) to warm
/// (large). Zero and non-finite values are dimmed.
fn log_color(v: f64) -> Color {
    if v == 0.0 || !v.is_finite() {
        return Color::DarkGray;
    }
    // Map exponent in [-6, 12] onto a blue → yellow → red gradient.
    let t = ((v.abs().log10() + 6.0) / 18.0).clamp(0.0, 1.0);
    let lerp = |a: (u8, u8, u8), b: (u8, u8, u8), u: f64| {
        let f = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * u).round() as u8;
        (f(a.0, b.0), f(a.1, b.1), f(a.2, b.2))
    };
    let (r, g, b) = if t < 0.5 {
        lerp((70, 130, 220), (200, 200, 90), t * 2.0)
    } else {
        lerp((200, 200, 90), (225, 80, 60), (t - 0.5) * 2.0)
    };
    Color::Rgb(r, g, b)
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
            InputKind::ColumnSearch => '-',
            InputKind::Filter => '&',
            InputKind::Goto => ':',
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
        let label = match search.scope {
            Some(col) => format!("  -{} @{}", search.query, app.data.column_names[col]),
            None => format!("  /{}", search.query),
        };
        spans.push(Span::styled(label, Style::new().fg(Color::Cyan)));
    }
    if let Some(q) = &app.filter_query {
        spans.push(Span::styled(
            format!("  &{q} ({}/{})", app.row_count(), app.data.nrows),
            Style::new().fg(Color::Green),
        ));
    }
    if let Some(s) = app.sort {
        let arrow = if s.dir == SortDir::Asc { "↑" } else { "↓" };
        spans.push(Span::styled(
            format!("  sort {arrow}{}", app.data.column_names[s.col]),
            Style::new().fg(Color::Blue),
        ));
    }
    if app.frozen_cols > 0 {
        spans.push(Span::styled(
            format!("  ❄{}", app.frozen_cols),
            Style::new().fg(Color::Magenta),
        ));
    }
    if let Some(st) = app.num_styles.get(&app.selected_col) {
        let dec = st.decimals.map(|n| format!(".{n}")).unwrap_or_default();
        let log = if st.log { " log" } else { "" };
        spans.push(Span::styled(
            format!("  num{dec}{log}"),
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
            " j/k/h/l move · / search · - col-search · n/N next · & filter · s sort · f freeze · q quit",
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
    let orig = app.selected_orig();
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

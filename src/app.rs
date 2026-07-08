use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::data::Dataset;

/// UI state: which cell is selected and where the viewport is scrolled to.
pub struct App {
    pub data: Dataset,
    pub row_offset: usize,
    pub col_offset: usize,
    pub selected_row: usize,
    pub selected_col: usize,
    /// Rows of table body the last render could fit; used for paging.
    pub viewport_rows: usize,
    pub should_quit: bool,
    /// Set if a render pass failed; surfaced after the terminal is restored.
    pub render_error: Option<String>,
}

impl App {
    pub fn new(data: Dataset) -> Self {
        Self {
            data,
            row_offset: 0,
            col_offset: 0,
            selected_row: 0,
            selected_col: 0,
            viewport_rows: 1,
            should_quit: false,
            render_error: None,
        }
    }

    fn last_row(&self) -> usize {
        self.data.nrows.saturating_sub(1)
    }

    fn last_col(&self) -> usize {
        self.data.ncols.saturating_sub(1)
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.viewport_rows.max(1);
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('c') if ctrl => self.should_quit = true,

            KeyCode::Char('j') | KeyCode::Down => self.move_row(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_row(-1),
            KeyCode::Char('h') | KeyCode::Left => self.move_col(-1),
            KeyCode::Char('l') | KeyCode::Right => self.move_col(1),

            KeyCode::Char('d') if ctrl => self.move_row(page as isize / 2),
            KeyCode::Char('u') if ctrl => self.move_row(-(page as isize / 2)),
            KeyCode::Char('f') if ctrl => self.move_row(page as isize),
            KeyCode::Char('b') if ctrl => self.move_row(-(page as isize)),
            KeyCode::PageDown => self.move_row(page as isize),
            KeyCode::PageUp => self.move_row(-(page as isize)),

            KeyCode::Char('g') | KeyCode::Home => self.selected_row = 0,
            KeyCode::Char('G') | KeyCode::End => self.selected_row = self.last_row(),
            KeyCode::Char('0') | KeyCode::Char('^') => {
                self.selected_col = 0;
                self.col_offset = 0;
            }
            KeyCode::Char('$') => self.selected_col = self.last_col(),
            _ => {}
        }
    }

    fn move_row(&mut self, delta: isize) {
        let target = (self.selected_row as isize + delta)
            .clamp(0, self.last_row() as isize) as usize;
        self.selected_row = target;
    }

    fn move_col(&mut self, delta: isize) {
        let target = (self.selected_col as isize + delta)
            .clamp(0, self.last_col() as isize) as usize;
        self.selected_col = target;
    }
}

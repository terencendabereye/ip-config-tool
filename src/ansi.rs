//! A small, pure (no Windows/FFI dependency) virtual terminal screen buffer.
//! Interprets a byte stream from a ConPTY session — plain text, SGR color
//! codes, and cursor movement/erase — into a grid of styled cells that
//! `gui.rs` renders into an `nwg::RichTextBox`. Deliberately not a full
//! terminal emulator (no scrollback buffer beyond the grid, no alternate
//! screen, no custom palettes) — just enough fidelity for typical device
//! CLI menus and SSH prompts, per the ConPTY issue's stated scope.

use vte::{Params, Parser, Perform};

/// The eight standard ANSI colors plus a "default" (theme foreground).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    #[default]
    Default,
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
}

impl Color {
    fn from_sgr(code: u16) -> Option<Color> {
        Some(match code {
            30 => Color::Black,
            31 => Color::Red,
            32 => Color::Green,
            33 => Color::Yellow,
            34 => Color::Blue,
            35 => Color::Magenta,
            36 => Color::Cyan,
            37 => Color::White,
            39 => Color::Default,
            _ => return None,
        })
    }

    /// RGB used when rendering into the RichTextBox. Roughly matches
    /// typical dark-terminal ANSI colors; `Default` means "don't override
    /// the control's normal text color".
    pub fn rgb(self) -> Option<(u8, u8, u8)> {
        match self {
            Color::Default => None,
            Color::Black => Some((0, 0, 0)),
            Color::Red => Some((205, 49, 49)),
            Color::Green => Some((13, 188, 121)),
            Color::Yellow => Some((229, 229, 16)),
            Color::Blue => Some((36, 114, 200)),
            Color::Magenta => Some((188, 63, 188)),
            Color::Cyan => Some((17, 168, 205)),
            Color::White => Some((229, 229, 229)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bold: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Cell { ch: ' ', fg: Color::Default, bold: false }
    }
}

/// A fixed-size `cols x rows` grid of styled cells plus a cursor position,
/// fed byte-by-byte from a PTY's output stream.
pub struct ScreenBuffer {
    cols: usize,
    rows: usize,
    cells: Vec<Cell>,
    cursor_row: usize,
    cursor_col: usize,
    cur_fg: Color,
    cur_bold: bool,
    parser: Parser,
    dirty: bool,
}

impl ScreenBuffer {
    pub fn new(cols: u16, rows: u16) -> Self {
        let cols = cols.max(1) as usize;
        let rows = rows.max(1) as usize;
        ScreenBuffer {
            cols,
            rows,
            cells: vec![Cell::default(); cols * rows],
            cursor_row: 0,
            cursor_col: 0,
            cur_fg: Color::Default,
            cur_bold: false,
            parser: Parser::new(),
            dirty: false,
        }
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Feeds raw PTY output bytes through the ANSI parser, updating the grid.
    pub fn feed(&mut self, bytes: &[u8]) {
        let mut performer = ScreenPerformer { screen: self };
        let mut parser = std::mem::replace(&mut performer.screen.parser, Parser::new());
        for &b in bytes {
            parser.advance(&mut performer, b);
        }
        performer.screen.parser = parser;
    }

    /// Resizes the grid, preserving existing content top-left-anchored
    /// (content that no longer fits is dropped — acceptable for this
    /// non-scrollback-preserving use case).
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let cols = cols.max(1) as usize;
        let rows = rows.max(1) as usize;
        let mut new_cells = vec![Cell::default(); cols * rows];
        for r in 0..self.rows.min(rows) {
            for c in 0..self.cols.min(cols) {
                new_cells[r * cols + c] = self.cells[r * self.cols + c];
            }
        }
        self.cols = cols;
        self.rows = rows;
        self.cells = new_cells;
        self.cursor_row = self.cursor_row.min(rows - 1);
        self.cursor_col = self.cursor_col.min(cols - 1);
        self.dirty = true;
    }

    /// Returns the current grid contents, one row at a time, as runs of
    /// same-styled cells — ready for the GUI layer to turn into
    /// `set_selection`/`set_char_format` calls.
    pub fn rows_as_runs(&self) -> Vec<Vec<(String, Color, bool)>> {
        let mut out = Vec::with_capacity(self.rows);
        for r in 0..self.rows {
            let row_cells = &self.cells[r * self.cols..(r + 1) * self.cols];
            let mut runs: Vec<(String, Color, bool)> = Vec::new();
            for cell in row_cells {
                match runs.last_mut() {
                    Some((text, fg, bold)) if *fg == cell.fg && *bold == cell.bold => {
                        text.push(cell.ch);
                    }
                    _ => runs.push((cell.ch.to_string(), cell.fg, cell.bold)),
                }
            }
            out.push(runs);
        }
        out
    }

    /// True if the buffer has changed since the last `clear_dirty` call.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::replace(&mut self.dirty, false)
    }

    fn put_char(&mut self, ch: char) {
        if self.cursor_col >= self.cols {
            self.cursor_col = 0;
            self.newline();
        }
        let idx = self.cursor_row * self.cols + self.cursor_col;
        self.cells[idx] = Cell { ch, fg: self.cur_fg, bold: self.cur_bold };
        self.cursor_col += 1;
        self.dirty = true;
    }

    fn newline(&mut self) {
        if self.cursor_row + 1 >= self.rows {
            // Scroll the grid up one row instead of growing it.
            self.cells.drain(0..self.cols);
            self.cells.resize(self.cols * self.rows, Cell::default());
        } else {
            self.cursor_row += 1;
        }
        self.dirty = true;
    }

    fn carriage_return(&mut self) {
        self.cursor_col = 0;
    }

    fn backspace(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        }
    }

    fn move_cursor(&mut self, row_delta: i32, col_delta: i32) {
        let new_row = (self.cursor_row as i32 + row_delta).clamp(0, self.rows as i32 - 1);
        let new_col = (self.cursor_col as i32 + col_delta).clamp(0, self.cols as i32 - 1);
        self.cursor_row = new_row as usize;
        self.cursor_col = new_col as usize;
    }

    fn set_cursor(&mut self, row: usize, col: usize) {
        self.cursor_row = row.min(self.rows - 1);
        self.cursor_col = col.min(self.cols - 1);
    }

    fn erase_line(&mut self, mode: u16) {
        let row_start = self.cursor_row * self.cols;
        let (from, to) = match mode {
            0 => (self.cursor_col, self.cols),      // cursor to end of line
            1 => (0, self.cursor_col + 1),           // start of line to cursor
            2 => (0, self.cols),                     // whole line
            _ => (0, 0),
        };
        for c in from..to.min(self.cols) {
            self.cells[row_start + c] = Cell::default();
        }
        self.dirty = true;
    }

    fn erase_screen(&mut self, mode: u16) {
        match mode {
            0 => {
                self.erase_line(0);
                for r in (self.cursor_row + 1)..self.rows {
                    let start = r * self.cols;
                    for c in 0..self.cols {
                        self.cells[start + c] = Cell::default();
                    }
                }
            }
            1 => {
                self.erase_line(1);
                for r in 0..self.cursor_row {
                    let start = r * self.cols;
                    for c in 0..self.cols {
                        self.cells[start + c] = Cell::default();
                    }
                }
            }
            2 | 3 => {
                self.cells.iter_mut().for_each(|c| *c = Cell::default());
                self.cursor_row = 0;
                self.cursor_col = 0;
            }
            _ => {}
        }
        self.dirty = true;
    }

    fn apply_sgr(&mut self, params: &Params) {
        let codes: Vec<u16> = params.iter().flat_map(|p| p.iter().copied()).collect();
        if codes.is_empty() {
            self.cur_fg = Color::Default;
            self.cur_bold = false;
            return;
        }
        let mut i = 0;
        while i < codes.len() {
            match codes[i] {
                0 => {
                    self.cur_fg = Color::Default;
                    self.cur_bold = false;
                }
                1 => self.cur_bold = true,
                22 => self.cur_bold = false,
                code if Color::from_sgr(code).is_some() => {
                    self.cur_fg = Color::from_sgr(code).unwrap();
                }
                _ => {}
            }
            i += 1;
        }
    }
}

struct ScreenPerformer<'a> {
    screen: &'a mut ScreenBuffer,
}

impl<'a> Perform for ScreenPerformer<'a> {
    fn print(&mut self, c: char) {
        self.screen.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => self.screen.newline(),
            b'\r' => self.screen.carriage_return(),
            0x08 => self.screen.backspace(), // backspace
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, _intermediates: &[u8], _ignore: bool, action: char) {
        let nth = |i: usize, default: i32| -> i32 {
            params.iter().nth(i).and_then(|p| p.first().copied()).map(|v| v as i32).filter(|&v| v != 0).unwrap_or(default)
        };
        match action {
            'A' => self.screen.move_cursor(-nth(0, 1), 0),
            'B' => self.screen.move_cursor(nth(0, 1), 0),
            'C' => self.screen.move_cursor(0, nth(0, 1)),
            'D' => self.screen.move_cursor(0, -nth(0, 1)),
            'H' | 'f' => {
                let row = nth(0, 1).max(1) as usize - 1;
                let col = nth(1, 1).max(1) as usize - 1;
                self.screen.set_cursor(row, col);
            }
            'J' => {
                let mode = params.iter().next().and_then(|p| p.first().copied()).unwrap_or(0);
                self.screen.erase_screen(mode);
            }
            'K' => {
                let mode = params.iter().next().and_then(|p| p.first().copied()).unwrap_or(0);
                self.screen.erase_line(mode);
            }
            'm' => self.screen.apply_sgr(params),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain_text(buf: &ScreenBuffer, row: usize) -> String {
        buf.rows_as_runs()[row].iter().map(|(s, _, _)| s.as_str()).collect::<String>().trim_end().to_string()
    }

    #[test]
    fn plain_text_is_written_left_to_right() {
        let mut buf = ScreenBuffer::new(20, 5);
        buf.feed(b"hello");
        assert_eq!(plain_text(&buf, 0), "hello");
    }

    #[test]
    fn newline_moves_to_next_row() {
        let mut buf = ScreenBuffer::new(20, 5);
        buf.feed(b"one\r\ntwo");
        assert_eq!(plain_text(&buf, 0), "one");
        assert_eq!(plain_text(&buf, 1), "two");
    }

    #[test]
    fn sgr_color_applies_to_following_text() {
        let mut buf = ScreenBuffer::new(20, 5);
        buf.feed(b"\x1b[31mred\x1b[0m plain");
        let runs = &buf.rows_as_runs()[0];
        assert_eq!(runs[0].0, "red");
        assert_eq!(runs[0].1, Color::Red);
        assert_eq!(runs[1].1, Color::Default);
    }

    #[test]
    fn sgr_reset_clears_color_and_bold() {
        let mut buf = ScreenBuffer::new(20, 5);
        buf.feed(b"\x1b[1;32mbold-green\x1b[0mnormal");
        let runs = &buf.rows_as_runs()[0];
        assert_eq!(runs[0].1, Color::Green);
        assert!(runs[0].2);
        assert_eq!(runs[1].1, Color::Default);
        assert!(!runs[1].2);
    }

    #[test]
    fn cursor_up_then_write_overwrites_previous_line() {
        let mut buf = ScreenBuffer::new(20, 5);
        buf.feed(b"one\r\ntwo");
        buf.feed(b"\x1b[1A\rXXX"); // cursor up 1, back to column 0, overwrite
        assert_eq!(plain_text(&buf, 0), "XXX");
        assert_eq!(plain_text(&buf, 1), "two");
    }

    #[test]
    fn erase_line_clears_from_cursor_to_end() {
        let mut buf = ScreenBuffer::new(20, 5);
        buf.feed(b"hello world");
        buf.feed(b"\r\x1b[5C\x1b[0K"); // move to col 5, erase to end of line
        assert_eq!(plain_text(&buf, 0), "hello");
    }

    #[test]
    fn resize_preserves_top_left_content() {
        let mut buf = ScreenBuffer::new(20, 5);
        buf.feed(b"hello");
        buf.resize(10, 3);
        assert_eq!(plain_text(&buf, 0), "hello");
        assert_eq!(buf.cols(), 10);
        assert_eq!(buf.rows(), 3);
    }

    #[test]
    fn scrolls_when_writing_past_last_row() {
        let mut buf = ScreenBuffer::new(10, 2);
        buf.feed(b"first\r\nsecond\r\nthird");
        assert_eq!(plain_text(&buf, 0), "second");
        assert_eq!(plain_text(&buf, 1), "third");
    }
}

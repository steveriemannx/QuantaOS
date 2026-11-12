//! ANSI terminal state: ransid emulation plus a cell grid for rendering.

use quanta_gpu_protocol::Rgba8;

#[derive(Clone, Copy)]
pub struct Cell {
    pub ch: char,
    pub fg: Rgba8,
    pub bg: Rgba8,
    pub bold: bool,
    pub underline: bool,
}

impl Cell {
    fn blank(bg: Rgba8, fg: Rgba8) -> Self {
        Self {
            ch: ' ',
            fg,
            bg,
            bold: false,
            underline: false,
        }
    }
}

pub struct Terminal {
    console: ransid::Console,
    grid: Vec<Cell>,
    alt_grid: Vec<Cell>,
    alternate: bool,
    pending_input: Vec<u8>,
}

impl Terminal {
    pub fn new(cols: usize, rows: usize) -> Self {
        let console = ransid::Console::new(cols, rows);
        let fg = cvt(&console.state.foreground);
        let bg = cvt(&console.state.background);
        let cell = Cell::blank(bg, fg);
        Self {
            console,
            grid: vec![cell; cols.saturating_mul(rows)],
            alt_grid: vec![cell; cols.saturating_mul(rows)],
            alternate: false,
            pending_input: Vec::new(),
        }
    }

    pub fn cols(&self) -> usize {
        self.console.state.w
    }

    pub fn rows(&self) -> usize {
        self.console.state.h
    }

    pub fn cell(&self, x: usize, y: usize) -> Option<&Cell> {
        if x >= self.cols() || y >= self.rows() {
            return None;
        }
        self.grid.get(y * self.cols() + x)
    }

    pub fn default_bg(&self) -> Rgba8 {
        cvt(&self.console.state.background_default)
    }

    pub fn cursor(&self) -> Option<(usize, usize)> {
        let state = &self.console.state;
        if !state.cursor || state.x >= state.w || state.y >= state.h {
            return None;
        }
        Some((state.x, state.y))
    }

    /// Feeds PTY output through the emulator.
    pub fn write(&mut self, bytes: &[u8]) {
        let Self {
            console,
            grid,
            alt_grid,
            alternate,
            pending_input,
        } = self;
        let cols = console.state.w;
        let rows = console.state.h;
        let clear_bg = cvt(&console.state.background);
        let clear_fg = cvt(&console.state.foreground);
        console.write(bytes, |event| match event {
            ransid::Event::Char {
                x,
                y,
                c,
                bold,
                underlined,
                color,
                ..
            } => {
                if x < cols && y < rows {
                    if let Some(cell) = grid.get_mut(y * cols + x) {
                        cell.ch = c;
                        cell.fg = cvt(&color);
                        cell.bold = bold;
                        cell.underline = underlined;
                    }
                }
            }
            ransid::Event::Rect { x, y, w, h, color } => {
                let bg = cvt(&color);
                for row in y..(y + h).min(rows) {
                    for col in x..(x + w).min(cols) {
                        if let Some(cell) = grid.get_mut(row * cols + col) {
                            cell.ch = ' ';
                            cell.bg = bg;
                        }
                    }
                }
            }
            ransid::Event::Input { data } => pending_input.extend_from_slice(data),
            ransid::Event::ScreenBuffer {
                alternate: next,
                clear,
            } => {
                if *alternate != next {
                    std::mem::swap(grid, alt_grid);
                }
                if clear {
                    for cell in grid.iter_mut() {
                        *cell = Cell::blank(clear_bg, clear_fg);
                    }
                }
                *alternate = next;
            }
            ransid::Event::Move {
                from_x,
                from_y,
                to_x,
                to_y,
                w,
                h,
            } => {
                let width = w.min(cols);
                let height = h.min(rows);
                for raw in 0..height {
                    let row = if from_y > to_y { raw } else { height - raw - 1 };
                    let from_row = from_y + row;
                    let to_row = to_y + row;
                    if from_row >= rows || to_row >= rows {
                        continue;
                    }
                    let from_col = from_x.min(cols);
                    let to_col = to_x.min(cols);
                    let len = width
                        .min(cols.saturating_sub(from_col))
                        .min(cols.saturating_sub(to_col));
                    if len == 0 {
                        continue;
                    }
                    let source = from_row * cols + from_col;
                    let target = to_row * cols + to_col;
                    grid.copy_within(source..source + len, target);
                }
            }
            ransid::Event::Resize { .. } | ransid::Event::Title { .. } => {}
        });
    }

    /// Terminal responses (e.g. cursor-position reports) for the PTY.
    pub fn take_input(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending_input)
    }
}

fn cvt(color: &ransid::Color) -> Rgba8 {
    let rgb = color.as_rgb();
    Rgba8::new(
        ((rgb >> 16) & 0xff) as u8,
        ((rgb >> 8) & 0xff) as u8,
        (rgb & 0xff) as u8,
        255,
    )
}

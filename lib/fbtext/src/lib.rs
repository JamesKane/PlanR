//! A text console on a linear framebuffer: the kernel's screen output until
//! there is a draw device (P6), and the only console on machines without a
//! serial port.
//!
//! Text is black on white in the font's 8×16 cells, scaled up on wide
//! screens, in a grid centered on the screen with at least a cell's margin
//! all round. The grid's characters are
//! kept in `cells`, so scrolling only redraws the cells that change and
//! never reads the framebuffer back (slow on write-combining memory).

#![cfg_attr(not(test), no_std)]

mod font;

pub use font::{Glyph, HEIGHT, WIDTH, glyph};

/// How a 32-bit pixel holds its color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Bytes R, G, B, x.
    Rgbx,
    /// Bytes B, G, R, x.
    Bgrx,
}

impl Format {
    #[must_use]
    pub fn pixel(self, r: u8, g: u8, b: u8) -> u32 {
        let (r, g, b) = (u32::from(r), u32::from(g), u32::from(b));
        match self {
            Format::Rgbx => r | (g << 8) | (b << 16),
            Format::Bgrx => b | (g << 8) | (r << 16),
        }
    }
}

/// A linear framebuffer of 32-bit pixels.
#[derive(Debug, Clone, Copy)]
pub struct Surface {
    pub base: *mut u32,
    pub width: usize,
    pub height: usize,
    /// Pixels from one row to the next.
    pub stride: usize,
    pub format: Format,
}

/// Screens at least this wide get each font pixel drawn as a 2×2 block,
/// twice as wide get 3×3, and so on.
const SCALE_STEP: usize = 1920;
const MAX_SCALE: usize = 4;
const TAB: usize = 8;

/// A text console drawing on a [`Surface`].
#[derive(Debug)]
pub struct Console<'a> {
    fb: Surface,
    cells: &'a mut [u8],
    cols: usize,
    rows: usize,
    scale: usize,
    /// Pixel position of the grid's top left corner.
    left: usize,
    top: usize,
    col: usize,
    row: usize,
    fg: u32,
    bg: u32,
}

// SAFETY: a `Console` has exclusive use of its framebuffer (the `new`
// contract) and owns its cells.
unsafe impl Send for Console<'_> {}

impl<'a> Console<'a> {
    /// Clears the screen and starts a console at the top left. The grid is
    /// as large as the screen allows inside a one-cell margin, but no more
    /// than `cells` holds. `None` if not even one cell fits.
    ///
    /// # Safety
    ///
    /// `fb.base` must be valid for writes and reads of `fb.stride *
    /// fb.height` pixels, used by nothing else, for `'a`.
    pub unsafe fn new(fb: Surface, cells: &'a mut [u8]) -> Option<Console<'a>> {
        if fb.base.is_null() || fb.stride < fb.width {
            return None;
        }
        let scale = (fb.width / SCALE_STEP).clamp(1, MAX_SCALE);
        let (cw, ch) = (WIDTH * scale, HEIGHT * scale);
        let cols = (fb.width / cw).saturating_sub(2).min(cells.len());
        let rows = (fb.height / ch).saturating_sub(2).min(cells.len().checked_div(cols)?);
        if rows == 0 {
            return None;
        }
        let mut c = Console {
            fb,
            cells,
            cols,
            rows,
            scale,
            left: (fb.width - cols * cw) / 2,
            top: (fb.height - rows * ch) / 2,
            col: 0,
            row: 0,
            fg: fb.format.pixel(0, 0, 0),
            bg: fb.format.pixel(0xff, 0xff, 0xff),
        };
        c.cells[..cols * rows].fill(b' ');
        c.fill(0, 0, fb.width, fb.height, c.bg);
        Some(c)
    }

    #[must_use]
    pub fn cols(&self) -> usize {
        self.cols
    }

    #[must_use]
    pub fn rows(&self) -> usize {
        self.rows
    }

    #[must_use]
    pub fn scale(&self) -> usize {
        self.scale
    }

    /// The cursor's column and row.
    #[must_use]
    pub fn cursor(&self) -> (usize, usize) {
        (self.col, self.row)
    }

    /// The characters on grid row `row`.
    #[must_use]
    pub fn text(&self, row: usize) -> &[u8] {
        let row = row.min(self.rows - 1);
        &self.cells[row * self.cols..(row + 1) * self.cols]
    }

    /// The pixel position of a cell's top left corner.
    #[must_use]
    pub fn cell_origin(&self, col: usize, row: usize) -> (usize, usize) {
        (
            self.left + col * WIDTH * self.scale,
            self.top + row * HEIGHT * self.scale,
        )
    }

    /// Reads one pixel back from the screen.
    #[must_use]
    pub fn pixel(&self, x: usize, y: usize) -> Option<u32> {
        (x < self.fb.width && y < self.fb.height).then(|| {
            // SAFETY: inside the framebuffer (`new` contract).
            unsafe { self.fb.base.add(y * self.fb.stride + x).read_volatile() }
        })
    }

    /// The color text is drawn in.
    #[must_use]
    pub fn ink(&self) -> u32 {
        self.fg
    }

    /// Writes text. Understands `\n`, `\r`, `\t` and backspace, ignores
    /// other control characters, and draws an empty box for each non-ASCII
    /// UTF-8 sequence.
    pub fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            match b {
                b'\n' => self.newline(),
                b'\r' => self.col = 0,
                b'\t' => {
                    let n = (TAB - self.col % TAB).min(self.cols - self.col);
                    for _ in 0..n {
                        self.put(b' ');
                    }
                }
                0x08 => self.col = self.col.saturating_sub(1),
                0x20..=0x7e | 0xc0..=0xff => self.put(b),
                // Other control characters, and UTF-8 continuation bytes
                // (the lead byte drew the box).
                _ => {}
            }
        }
    }

    fn put(&mut self, b: u8) {
        // Wrap only when another character arrives, so a line that exactly
        // fills the width is not followed by a blank one.
        if self.col == self.cols {
            self.newline();
        }
        let b = if b.is_ascii() { b } else { 0xff };
        self.cells[self.row * self.cols + self.col] = b;
        self.draw(self.col, self.row, b);
        self.col += 1;
    }

    fn newline(&mut self) {
        self.col = 0;
        if self.row + 1 < self.rows {
            self.row += 1;
        } else {
            self.scroll();
        }
    }

    /// Moves every row up one, redrawing only the cells that change.
    fn scroll(&mut self) {
        for r in 0..self.rows {
            for c in 0..self.cols {
                let new = if r + 1 < self.rows {
                    self.cells[(r + 1) * self.cols + c]
                } else {
                    b' '
                };
                let i = r * self.cols + c;
                if self.cells[i] != new {
                    self.cells[i] = new;
                    self.draw(c, r, new);
                }
            }
        }
    }

    fn draw(&mut self, col: usize, row: usize, c: u8) {
        let g = glyph(c);
        let (x0, y0) = self.cell_origin(col, row);
        let s = self.scale;
        for (gy, &bits) in g.iter().enumerate() {
            for sy in 0..s {
                let line = (y0 + gy * s + sy) * self.fb.stride + x0;
                for gx in 0..WIDTH {
                    let px = if bits & (0x80 >> gx) != 0 { self.fg } else { self.bg };
                    for sx in 0..s {
                        // SAFETY: the grid lies inside the framebuffer.
                        unsafe { self.fb.base.add(line + gx * s + sx).write_volatile(px) };
                    }
                }
            }
        }
    }

    fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, px: u32) {
        for yy in y..y + h {
            for xx in x..x + w {
                // SAFETY: callers stay inside the framebuffer.
                unsafe { self.fb.base.add(yy * self.fb.stride + xx).write_volatile(px) };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WHITE: u32 = 0x00ff_ffff;

    struct Screen {
        px: Vec<u32>,
        cells: Vec<u8>,
        width: usize,
        height: usize,
        stride: usize,
    }

    impl Screen {
        fn new(width: usize, height: usize, stride: usize) -> Screen {
            Screen {
                px: vec![0x1234_5678; stride * height],
                cells: vec![0; 4096],
                width,
                height,
                stride,
            }
        }

        fn console(&mut self) -> Console<'_> {
            let fb = Surface {
                base: self.px.as_mut_ptr(),
                width: self.width,
                height: self.height,
                stride: self.stride,
                format: Format::Bgrx,
            };
            // SAFETY: the test owns the pixels and cells.
            unsafe { Console::new(fb, &mut self.cells) }.unwrap()
        }
    }

    fn row_str(c: &Console<'_>, row: usize) -> String {
        String::from_utf8_lossy(c.text(row)).trim_end().to_string()
    }

    /// True if the cell at (col, row) shows glyph `ch`.
    fn shows(c: &Console<'_>, col: usize, row: usize, ch: u8) -> bool {
        let (x0, y0) = c.cell_origin(col, row);
        let g = glyph(ch);
        (0..HEIGHT).all(|gy| {
            (0..WIDTH).all(|gx| {
                let ink = g[gy] & (0x80 >> gx) != 0;
                let want = if ink { 0 } else { WHITE };
                c.pixel(x0 + gx * c.scale(), y0 + gy * c.scale()) == Some(want)
            })
        })
    }

    #[test]
    fn grid_is_centered_and_screen_cleared() {
        // 116×82: room for 14×5 cells of 8×16; 12×3 inside the margin,
        // leaving 20 and 34 pixels to split either side.
        let mut s = Screen::new(116, 82, 120);
        {
            let c = s.console();
            assert_eq!((c.cols(), c.rows(), c.scale()), (12, 3, 1));
            assert_eq!(c.cell_origin(0, 0), (10, 17));
            assert!((0..82).all(|y| (0..116).all(|x| c.pixel(x, y) == Some(WHITE))));
        }
        // Padding past the width is not touched.
        assert_eq!(s.px[116], 0x1234_5678);
    }

    #[test]
    fn draws_glyphs() {
        let mut s = Screen::new(116, 82, 116);
        let mut c = s.console();
        c.write(b"Hi!");
        assert!(shows(&c, 0, 0, b'H') && shows(&c, 1, 0, b'i') && shows(&c, 2, 0, b'!'));
        assert!(shows(&c, 3, 0, b' '));
        assert_eq!(c.cursor(), (3, 0));
    }

    #[test]
    fn wraps_and_scrolls() {
        let mut s = Screen::new(116, 82, 116); // 12×3
        let mut c = s.console();
        c.write(b"first\nsecond\nthird\nfourth\n");
        assert_eq!(row_str(&c, 0), "third");
        assert_eq!(row_str(&c, 1), "fourth");
        assert_eq!(row_str(&c, 2), "");
        assert!(shows(&c, 0, 0, b't') && shows(&c, 5, 1, b'h') && shows(&c, 5, 0, b' '));
        c.write(b"abcdefghijklmnop");
        assert_eq!(row_str(&c, 1), "abcdefghijkl");
        assert_eq!(row_str(&c, 2), "mnop");
        assert!(shows(&c, 3, 2, b'p'));
    }

    #[test]
    fn exact_width_line_does_not_add_a_blank_line() {
        let mut s = Screen::new(116, 82, 116);
        let mut c = s.console();
        c.write(b"abcdefghijkl\nnext");
        assert_eq!(row_str(&c, 0), "abcdefghijkl");
        assert_eq!(row_str(&c, 1), "next");
    }

    #[test]
    fn control_characters() {
        let mut s = Screen::new(116, 82, 116);
        let mut c = s.console();
        c.write(b"ab\tc\rX\x08\x08Y\x1b");
        assert_eq!(row_str(&c, 0), "Yb      c");
        assert_eq!(c.cursor(), (1, 0));
    }

    #[test]
    fn non_ascii_is_one_box_per_character() {
        let mut s = Screen::new(116, 82, 116);
        let mut c = s.console();
        c.write("é→x".as_bytes());
        assert_eq!(c.cursor(), (3, 0));
        assert!(shows(&c, 0, 0, 0xff) && shows(&c, 1, 0, 0xff) && shows(&c, 2, 0, b'x'));
    }

    #[test]
    fn wide_screens_scale_up() {
        let mut s = Screen::new(3840, 128, 3840);
        let c = s.console();
        assert_eq!(c.scale(), 2);
        assert_eq!((c.cols(), c.rows()), (238, 2));
    }

    #[test]
    fn grid_limited_by_cells() {
        let mut s = Screen::new(400, 400, 400); // 48×23 inside the margin
        s.cells.truncate(100);
        let c = s.console();
        assert_eq!((c.cols(), c.rows()), (48, 2));
    }

    #[test]
    fn pixel_formats() {
        assert_eq!(Format::Rgbx.pixel(1, 2, 3), 0x0003_0201);
        assert_eq!(Format::Bgrx.pixel(1, 2, 3), 0x0001_0203);
    }
}

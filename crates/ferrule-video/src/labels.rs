//! Timecodes burned onto a contact sheet without a font. Some static ffmpeg
//! builds have no `drawtext` and no fontconfig (the owner's server's has
//! neither), so the digits are drawn here from a 5×7 table into one
//! transparent image the size of the sheet, in PAM (`P7`, RGB_ALPHA),
//! which ffmpeg reads as one more input and lays over the sheet with
//! `overlay`. No font, no licence, no path in a filter string.

/// Rows of a 5×7 glyph, top first, the high bit of the low five the left
/// column.
fn glyph(c: char) -> Option<[u8; 7]> {
    Some(match c {
        '0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        '1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        '2' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        '3' => [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
        '4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        '5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        '6' => [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        '7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        '9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
        ':' => [0x00, 0x0C, 0x0C, 0x00, 0x0C, 0x0C, 0x00],
        '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C],
        ' ' => [0; 7],
        _ => return None,
    })
}

/// The layout of a sheet ffmpeg's `tile` filter makes: `cols` across, each
/// tile `tile_w`×`tile_h`, `padding` pixels between tiles, no margin.
#[derive(Debug, Clone, Copy)]
pub struct Sheet {
    pub cols: u32,
    pub rows: u32,
    pub tile_w: u32,
    pub tile_h: u32,
    pub padding: u32,
}

impl Sheet {
    pub fn width(&self) -> u32 {
        self.cols * self.tile_w + self.cols.saturating_sub(1) * self.padding
    }

    pub fn height(&self) -> u32 {
        self.rows * self.tile_h + self.rows.saturating_sub(1) * self.padding
    }

    /// A PAM image the size of the sheet, transparent but for `texts[i]`
    /// in white on a dark box at the bottom left of tile `i`. Characters
    /// other than digits, `:`, `.` and space are left out.
    pub fn overlay(&self, texts: &[String]) -> Vec<u8> {
        let (w, h) = (self.width() as usize, self.height() as usize);
        let mut px = vec![0u8; w * h * 4];
        let mut put = |x: usize, y: usize, rgba: [u8; 4]| {
            if x < w && y < h {
                let i = (y * w + x) * 4;
                px[i..i + 4].copy_from_slice(&rgba);
            }
        };
        for (i, text) in texts.iter().enumerate() {
            let glyphs: Vec<[u8; 7]> = text.chars().filter_map(glyph).collect();
            if glyphs.is_empty() || i as u32 >= self.cols * self.rows {
                continue;
            }
            let n = glyphs.len() as u32;
            // Scale with the tile, and down until the label fits across it.
            let mut s = (self.tile_h / 90).clamp(2, 8);
            while s > 1 && (n * 6 * s + 2 * s) > self.tile_w.saturating_sub(4) {
                s -= 1;
            }
            let (col, row) = (i as u32 % self.cols, i as u32 / self.cols);
            let x0 = col * (self.tile_w + self.padding) + 2 * s;
            let y0 =
                (row * (self.tile_h + self.padding) + self.tile_h).saturating_sub(2 * s + 9 * s);
            let (box_w, box_h) = (n * 6 * s + s, 9 * s);
            for y in 0..box_h {
                for x in 0..box_w {
                    put((x0 + x) as usize, (y0 + y) as usize, [0, 0, 0, 170]);
                }
            }
            for (k, g) in glyphs.iter().enumerate() {
                let gx = x0 + s + k as u32 * 6 * s;
                for (r, bits) in g.iter().enumerate() {
                    for c in 0..5u32 {
                        if bits & (0x10 >> c) == 0 {
                            continue;
                        }
                        for dy in 0..s {
                            for dx in 0..s {
                                put(
                                    (gx + c * s + dx) as usize,
                                    (y0 + s + r as u32 * s + dy) as usize,
                                    [255, 255, 255, 255],
                                );
                            }
                        }
                    }
                }
            }
        }
        let mut out =
            format!("P7\nWIDTH {w}\nHEIGHT {h}\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR\n")
                .into_bytes();
        out.extend_from_slice(&px);
        out
    }
}

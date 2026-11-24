use crate::appearance::Palette;
use gpui::{FontStyle, FontWeight, HighlightStyle, StyledText, rgb};
use std::ops::Range;

fn color(value: vt100::Color, default: u32) -> u32 {
    match value {
        vt100::Color::Default => default,
        vt100::Color::Rgb(r, g, b) => u32::from_be_bytes([0, r, g, b]),
        vt100::Color::Idx(i @ 0..=15) => [
            0x23252e, 0xd4777d, 0x94c58b, 0xe3bd75, 0x81a7db, 0xb997cb, 0x77bcc7, 0xcbccd2,
            0x787c89, 0xff999f, 0xb4df9e, 0xf7d99b, 0xa1c4fa, 0xd7b4e9, 0x9ad8de, 0xf5f4f0,
        ][usize::from(i)],
        vt100::Color::Idx(i @ 16..=231) => {
            let n = i - 16;
            let channel = |v: u8| if v == 0 { 0 } else { 55 + 40 * v };
            u32::from_be_bytes([0, channel(n / 36), channel(n / 6 % 6), channel(n % 6)])
        }
        vt100::Color::Idx(i) => u32::from_be_bytes([
            0,
            8 + 10 * (i - 232),
            8 + 10 * (i - 232),
            8 + 10 * (i - 232),
        ]),
    }
}

pub(super) fn rows(screen: &vt100::Screen, p: Palette) -> Vec<StyledText> {
    let (height, width) = screen.size();
    let cursor =
        (!screen.hide_cursor() && screen.scrollback() == 0).then(|| screen.cursor_position());
    (0..height)
        .map(|y| {
            let mut text = String::new();
            let mut ranges: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
            for x in 0..width {
                let Some(cell) = screen.cell(y, x) else {
                    continue;
                };
                if cell.is_wide_continuation() {
                    continue;
                }
                let caret = cursor == Some((y, x));
                let mut fg = color(cell.fgcolor(), p.fg);
                let mut bg = color(cell.bgcolor(), p.bg);
                if cell.inverse() {
                    std::mem::swap(&mut fg, &mut bg);
                }
                if caret {
                    fg = p.bg;
                    bg = p.accent;
                }
                let start = text.len();
                text.push_str(if cell.contents().is_empty() {
                    " "
                } else {
                    cell.contents()
                });
                let style = HighlightStyle {
                    color: (fg != p.fg).then(|| rgb(fg).into()),
                    background_color: (bg != p.bg).then(|| rgb(bg).into()),
                    font_weight: cell.bold().then_some(FontWeight::BOLD),
                    font_style: cell.italic().then_some(FontStyle::Italic),
                    ..Default::default()
                };
                if style == HighlightStyle::default() {
                    continue;
                }
                if let Some((r, prev)) = ranges.last_mut()
                    && *prev == style
                    && r.end == start
                {
                    r.end = text.len();
                } else {
                    ranges.push((start..text.len(), style));
                }
            }
            StyledText::new(text).with_highlights(ranges)
        })
        .collect()
}

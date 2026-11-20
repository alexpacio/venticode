use ftui::text::display_width;
use unicode_segmentation::UnicodeSegmentation;

pub struct Row<'a> {
    pub text: &'a str,
    start: usize,
}

/// Keep the insertion point on screen even when a line exactly fills the editor.
pub fn rows(text: &str, width: usize) -> Vec<Row<'_>> {
    let width = width.max(1);
    let mut result = Vec::new();
    let mut start = 0;
    let mut cells = 0;
    for (index, grapheme) in text.grapheme_indices(true) {
        let size = display_width(grapheme);
        if grapheme == "\n" {
            result.push(Row {
                text: &text[start..index],
                start,
            });
            start = index + 1;
            cells = 0;
        } else {
            if cells > 0 && cells + size > width {
                result.push(Row {
                    text: &text[start..index],
                    start,
                });
                start = index;
                cells = 0;
            }
            cells += size;
        }
    }
    result.push(Row {
        text: &text[start..],
        start,
    });
    if cells >= width {
        result.push(Row {
            text: "",
            start: text.len(),
        });
    }
    result
}

pub fn position(rows: &[Row], caret: usize) -> (usize, usize) {
    let index = rows
        .partition_point(|row| row.start <= caret)
        .saturating_sub(1);
    let row = &rows[index];
    (index, display_width(&row.text[..caret - row.start]))
}

pub fn vertical(text: &str, caret: usize, width: usize, down: bool) -> Option<usize> {
    let rows = rows(text, width);
    let (index, col) = position(&rows, caret);
    let target = if down {
        index + 1
    } else {
        index.checked_sub(1)?
    };
    let row = rows.get(target)?;
    let mut pos = row.start;
    let mut cells = 0;
    for g in row.text.graphemes(true) {
        cells += display_width(g);
        if cells > col
            || rows
                .get(target + 1)
                .is_some_and(|next| next.start == pos + g.len())
        {
            break;
        }
        pos += g.len();
    }
    Some(pos)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wraps_at_cell_boundaries_and_tracks_caret() {
        let text = "ab界e\u{301}\nxyz";
        let rows = rows(text, 3);
        assert_eq!(
            rows.iter().map(|r| r.text).collect::<Vec<_>>(),
            ["ab", "界e\u{301}", "xyz", ""]
        );
        assert_eq!(position(&rows, 5), (1, 2));
        assert_eq!(vertical(text, 5, 3, true), Some(text.len() - 1));
    }
}

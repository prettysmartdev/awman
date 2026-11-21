//! One byte-to-screen layout for multiline rendering and cursor navigation.
use std::ops::Range;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub struct WrappedText<'a> {
    text: &'a str,
    width: usize,
    pub rows: Vec<Range<usize>>,
}

impl<'a> WrappedText<'a> {
    pub fn new(text: &'a str, width: usize) -> Self {
        let width = width.max(1);
        let mut rows = Vec::new();
        let mut offset = 0;
        for line in text.split('\n') {
            let end = offset + line.len();
            let mut start = offset;
            while start < end {
                let mut columns = 0;
                let mut last_break = None;
                let mut split = end;
                let mut previous_whitespace = false;
                for (i, ch) in text[start..end].char_indices() {
                    let ch_width = ch.width().unwrap_or(0);
                    // Keep a word's trailing delimiters with it, even at the
                    // right edge, rather than inserting a whitespace-only row
                    // between two words that each fit the field.
                    if columns + ch_width > width
                        && i > 0
                        && (!ch.is_whitespace() || previous_whitespace)
                    {
                        split = last_break.unwrap_or(start + i);
                        break;
                    }
                    columns += ch_width;
                    if ch.is_whitespace() {
                        last_break = Some(start + i + ch.len_utf8());
                    }
                    previous_whitespace = ch.is_whitespace();
                }
                rows.push(start..split);
                start = split;
            }
            // Empty logical lines and a caret after an exactly full row need
            // a real screen row, including before an explicit newline.
            if line.is_empty()
                || rows
                    .last()
                    .is_some_and(|row| text[row.clone()].width() >= width)
            {
                rows.push(end..end);
            }
            offset = end + 1;
        }
        Self { text, width, rows }
    }

    pub fn line(&self, row: usize) -> &'a str {
        &self.text[self.rows[row].clone()]
    }

    pub fn cursor_position(&self, cursor: usize) -> (usize, usize) {
        // A soft boundary belongs to the following row; a newline belongs to
        // the row before it. Rows retain byte offsets even for multibyte text.
        let row = self
            .rows
            .iter()
            .rposition(|range| range.start <= cursor)
            .unwrap_or(0);
        let range = &self.rows[row];
        (
            row,
            self.text[range.start..cursor.min(range.end)]
                .width()
                .min(self.width - 1),
        )
    }

    pub fn cursor_at(&self, row: usize, column: usize) -> usize {
        let range = &self.rows[row];
        let mut cursor = range.start;
        let mut columns = 0;
        for (i, ch) in self.line(row).char_indices() {
            columns += ch.width().unwrap_or(0);
            if columns > column {
                break;
            }
            let next = range.start + i + ch.len_utf8();
            // The end of a soft-wrapped row is the next row's first column.
            // Clamp to this row instead of accidentally moving twice.
            if next == range.end && self.rows.get(row + 1).is_some_and(|r| r.start == next) {
                break;
            }
            cursor = next;
        }
        cursor
    }
}

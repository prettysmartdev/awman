//! Shared single-line and multiline text editing widget.

use std::cell::Cell;
use unicode_width::UnicodeWidthStr;

mod wrapped;
pub(super) use wrapped::WrappedText;

pub struct TextEdit {
    pub text: String,
    pub cursor: usize,
    pub multiline: bool,
    /// Published by the renderer, which knows the actual inner field width.
    wrap_width: Cell<usize>,
    preferred_column: Option<usize>,
}
impl TextEdit {
    pub fn new(multiline: bool) -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            multiline,
            wrap_width: Cell::new(usize::MAX),
            preferred_column: None,
        }
    }

    pub fn set_text(&mut self, text: &str) {
        self.preferred_column = None;
        self.text = text.to_string();
        self.cursor = self.text.len();
    }

    pub fn insert_char(&mut self, ch: char) {
        self.preferred_column = None;
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    pub fn insert_newline(&mut self) {
        if self.multiline {
            self.insert_char('\n');
        }
    }

    pub fn backspace(&mut self) {
        self.preferred_column = None;
        if self.cursor > 0 {
            let prev = self.prev_char_boundary();
            self.text.drain(prev..self.cursor);
            self.cursor = prev;
        }
    }

    pub fn delete(&mut self) {
        self.preferred_column = None;
        if self.cursor < self.text.len() {
            let next = self.next_char_boundary();
            self.text.drain(self.cursor..next);
        }
    }

    pub fn backspace_word(&mut self) {
        self.preferred_column = None;
        if self.cursor == 0 {
            return;
        }
        let start = self.word_boundary_left();
        self.text.drain(start..self.cursor);
        self.cursor = start;
    }

    pub fn move_left(&mut self) {
        self.preferred_column = None;
        if self.cursor > 0 {
            self.cursor = self.prev_char_boundary();
        }
    }

    pub fn move_right(&mut self) {
        self.preferred_column = None;
        if self.cursor < self.text.len() {
            self.cursor = self.next_char_boundary();
        }
    }

    pub fn move_home(&mut self) {
        self.preferred_column = None;
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.preferred_column = None;
        self.cursor = self.text.len();
    }

    pub fn move_word_left(&mut self) {
        self.preferred_column = None;
        self.cursor = self.word_boundary_left();
    }

    pub fn move_word_right(&mut self) {
        self.preferred_column = None;
        self.cursor = self.word_boundary_right();
    }

    pub fn display_width(&self) -> usize {
        UnicodeWidthStr::width(self.text.as_str())
    }

    pub(super) fn wrapped(&self, width: usize) -> WrappedText<'_> {
        self.wrap_width.set(width.max(1));
        WrappedText::new(&self.text, width.max(1))
    }

    /// Move by displayed rows, retaining the desired column across short rows.
    pub(super) fn move_vertical(&mut self, direction: i32) {
        if !self.multiline {
            return;
        }
        let layout = WrappedText::new(&self.text, self.wrap_width.get());
        let (row, column) = layout.cursor_position(self.cursor);
        let column = *self.preferred_column.get_or_insert(column);
        let target = row
            .saturating_add_signed(direction as isize)
            .min(layout.rows.len() - 1);
        if target != row {
            self.cursor = layout.cursor_at(target, column);
        }
    }

    fn prev_char_boundary(&self) -> usize {
        let mut pos = self.cursor.saturating_sub(1);
        while pos > 0 && !self.text.is_char_boundary(pos) {
            pos -= 1;
        }
        pos
    }

    fn next_char_boundary(&self) -> usize {
        let mut pos = self.cursor + 1;
        while pos < self.text.len() && !self.text.is_char_boundary(pos) {
            pos += 1;
        }
        pos.min(self.text.len())
    }

    fn word_boundary_left(&self) -> usize {
        let bytes = self.text.as_bytes();
        let mut pos = self.cursor;
        while pos > 0 && bytes[pos - 1] == b' ' {
            pos -= 1;
        }
        while pos > 0 && bytes[pos - 1] != b' ' {
            pos -= 1;
        }
        pos
    }

    fn word_boundary_right(&self) -> usize {
        let bytes = self.text.as_bytes();
        let mut pos = self.cursor;
        while pos < bytes.len() && bytes[pos] != b' ' {
            pos += 1;
        }
        while pos < bytes.len() && bytes[pos] == b' ' {
            pos += 1;
        }
        pos
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_words_and_keeps_source_bytes_and_explicit_empty_lines() {
        let text = "hello world\n\n界界 café\nabcdefghijk\n";
        let layout = WrappedText::new(text, 6);
        let lines: Vec<_> = (0..layout.rows.len()).map(|row| layout.line(row)).collect();
        assert_eq!(
            lines,
            ["hello ", "world", "", "界界 ", "café", "abcdef", "ghijk", ""]
        );
        assert_eq!(layout.cursor_position(text.find("café").unwrap()), (4, 0));
        assert_eq!(layout.cursor_position(text.len()), (7, 0));
        // A delimiter after an exactly full word must not create a blank row.
        let exact = WrappedText::new("hello world", 5);
        assert_eq!(
            (0..exact.rows.len())
                .map(|r| exact.line(r))
                .collect::<Vec<_>>(),
            ["hello ", "world", ""]
        );
    }

    #[test]
    fn vertical_navigation_tracks_visual_columns_across_short_and_wide_rows() {
        let mut editor = TextEdit::new(true);
        editor.set_text("abcdef ghijkl\nx\n界界ab\n");
        editor.wrapped(7);
        editor.move_home();
        for _ in 0..5 {
            editor.move_right();
        }
        editor.move_vertical(1);
        assert_eq!(&editor.text[editor.cursor..], "l\nx\n界界ab\n");
        editor.move_vertical(1);
        assert_eq!(&editor.text[editor.cursor..], "\n界界ab\n");
        editor.move_vertical(1);
        assert_eq!(&editor.text[editor.cursor..], "b\n");
        editor.move_vertical(-2);
        assert_eq!(&editor.text[editor.cursor..], "l\nx\n界界ab\n");
        editor.move_left(); // horizontal movement resets the desired column
        editor.move_vertical(2);
        assert_eq!(&editor.text[editor.cursor..], "ab\n");
        editor.insert_char('!');
        assert!(editor.text.contains("界界!ab"));
    }

    #[test]
    fn resize_full_rows_and_empty_fields_have_valid_caret_positions() {
        let mut editor = TextEdit::new(true);
        editor.set_text("abcdefgh");
        assert_eq!(editor.wrapped(4).cursor_position(editor.cursor), (2, 0));
        editor.move_vertical(-1);
        assert_eq!(editor.cursor, 4);
        editor.wrapped(3);
        editor.move_vertical(-1);
        assert_eq!(editor.cursor, 0);
        editor.move_end();
        editor.move_vertical(-1);
        assert_eq!(editor.cursor, 5);
        editor.set_text("");
        editor.wrapped(0);
        editor.move_vertical(1);
        assert_eq!(editor.cursor, 0);
        editor.set_text("界");
        editor.wrapped(1);
        editor.move_vertical(-1);
        assert_eq!(editor.cursor, 0);
    }
}

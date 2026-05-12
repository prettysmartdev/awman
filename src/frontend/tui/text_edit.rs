//! Shared single-line and multiline text editing widget.

use unicode_width::UnicodeWidthStr;

pub struct TextEdit {
    pub text: String,
    pub cursor: usize,
    pub multiline: bool,
}

impl TextEdit {
    pub fn new(multiline: bool) -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            multiline,
        }
    }

    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_string();
        self.cursor = self.text.len();
    }

    pub fn insert_char(&mut self, ch: char) {
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    pub fn insert_newline(&mut self) {
        if self.multiline {
            self.insert_char('\n');
        }
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            let prev = self.prev_char_boundary();
            self.text.drain(prev..self.cursor);
            self.cursor = prev;
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            let next = self.next_char_boundary();
            self.text.drain(self.cursor..next);
        }
    }

    pub fn backspace_word(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = self.word_boundary_left();
        self.text.drain(start..self.cursor);
        self.cursor = start;
    }

    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = self.prev_char_boundary();
        }
    }

    pub fn move_right(&mut self) {
        if self.cursor < self.text.len() {
            self.cursor = self.next_char_boundary();
        }
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.text.len();
    }

    pub fn move_word_left(&mut self) {
        self.cursor = self.word_boundary_left();
    }

    pub fn move_word_right(&mut self) {
        self.cursor = self.word_boundary_right();
    }

    pub fn display_width(&self) -> usize {
        UnicodeWidthStr::width(self.text.as_str())
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
    fn insert_and_backspace() {
        let mut e = TextEdit::new(false);
        e.insert_char('a');
        e.insert_char('b');
        assert_eq!(e.text, "ab");
        e.backspace();
        assert_eq!(e.text, "a");
    }

    #[test]
    fn cursor_movement() {
        let mut e = TextEdit::new(false);
        e.set_text("hello");
        e.move_home();
        assert_eq!(e.cursor, 0);
        e.move_end();
        assert_eq!(e.cursor, 5);
        e.move_left();
        assert_eq!(e.cursor, 4);
        e.move_right();
        assert_eq!(e.cursor, 5);
    }

    #[test]
    fn word_movement() {
        let mut e = TextEdit::new(false);
        e.set_text("hello world test");
        e.move_home();
        e.move_word_right();
        assert_eq!(e.cursor, 6);
        e.move_word_left();
        assert_eq!(e.cursor, 0);
    }

    #[test]
    fn delete_removes_char_at_cursor() {
        let mut e = TextEdit::new(false);
        e.set_text("abc");
        e.move_home();
        e.delete();
        assert_eq!(e.text, "bc");
        assert_eq!(e.cursor, 0);
    }

    #[test]
    fn delete_at_end_is_noop() {
        let mut e = TextEdit::new(false);
        e.set_text("ab");
        e.move_end();
        e.delete();
        assert_eq!(e.text, "ab");
    }

    #[test]
    fn backspace_word_removes_preceding_word() {
        let mut e = TextEdit::new(false);
        e.set_text("hello world");
        e.move_end();
        e.backspace_word();
        assert_eq!(e.text, "hello ");
    }

    #[test]
    fn backspace_word_at_start_is_noop() {
        let mut e = TextEdit::new(false);
        e.set_text("hello");
        e.move_home();
        e.backspace_word();
        assert_eq!(e.text, "hello");
    }

    #[test]
    fn insert_newline_only_in_multiline_mode() {
        let mut single = TextEdit::new(false);
        single.insert_newline();
        assert_eq!(single.text, "", "single-line must not insert newline");

        let mut multi = TextEdit::new(true);
        multi.insert_newline();
        assert_eq!(multi.text, "\n", "multiline must insert newline");
    }

    #[test]
    fn display_width_ascii() {
        let mut e = TextEdit::new(false);
        e.set_text("hello");
        assert_eq!(e.display_width(), 5);
    }

    #[test]
    fn display_width_empty() {
        let e = TextEdit::new(false);
        assert_eq!(e.display_width(), 0);
    }

    #[test]
    fn set_text_places_cursor_at_end() {
        let mut e = TextEdit::new(false);
        e.set_text("hello");
        assert_eq!(e.cursor, 5);
    }

    #[test]
    fn move_word_right_from_middle_of_word() {
        let mut e = TextEdit::new(false);
        e.set_text("hello world");
        e.move_home();
        e.move_right();
        e.move_right();
        assert_eq!(e.cursor, 2);
        e.move_word_right();
        assert_eq!(
            e.cursor, 6,
            "word-right from mid-word must jump to start of next word"
        );
    }
}

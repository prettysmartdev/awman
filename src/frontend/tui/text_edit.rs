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

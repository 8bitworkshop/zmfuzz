//! A `UI` that keeps every `print` call as its own string.
//!
//! `search.js` tokenizes *printed chunks*, not words: `game.print` is handed
//! one string per print opcode, and the fuzzer turns a whole chunk into a token
//! if it is long enough (`usewords`), so "You are standing in an open field"
//! becomes one token, and `BaseUI`'s chunk coalescing would blur that.
//!
//! It also means no window bookkeeping: the fuzzer never looks at the screen.

use encrusted_heart::traits::{TextStyle, Window, UI};

pub struct TokenUI {
    prints: Vec<String>,
    status_line: Option<(String, String)>,
}

impl TokenUI {
    pub fn new() -> TokenUI {
        TokenUI {
            prints: Vec::new(),
            status_line: None,
        }
    }

    /// Text printed since the last call, one entry per print opcode.
    pub fn drain_prints(&mut self) -> Vec<String> {
        std::mem::take(&mut self.prints)
    }
}

impl UI for TokenUI {
    fn print(&mut self, text: &str, _style: TextStyle) {
        if !text.is_empty() {
            self.prints.push(text.to_string());
        }
    }

    fn debug(&mut self, _text: &str) {}

    fn print_object(&mut self, object: &str) {
        self.print(object, TextStyle::default());
    }

    fn set_status_bar(&mut self, left: &str, right: &str) {
        self.status_line = Some((left.to_string(), right.to_string()));
    }

    fn set_window(&mut self, _window: Window) {}

    fn split_window(&mut self, _lines: u16) {}
    fn erase_window(&mut self, _window: Window) {}
    fn set_cursor(&mut self, _line: u16, _column: u16) {}
}

//! A continuously drained, bounded PTY stream and a real VT screen model.
use crate::{
    error::{Code, Failure, Result},
    files::hash,
};
use base64::Engine;
use serde::Serialize;
use std::collections::VecDeque;

#[derive(Default)]
struct Responses {
    bytes: Vec<Vec<u8>>,
}
impl vt100::Callbacks for Responses {
    fn unhandled_csi(
        &mut self,
        s: &mut vt100::Screen,
        i1: Option<u8>,
        _: Option<u8>,
        p: &[&[u16]],
        c: char,
    ) {
        if self.bytes.len() >= 16 {
            return;
        }
        let first = p.first().and_then(|p| p.first()).copied().unwrap_or(0);
        let reply = match (i1, c, first) {
            (None, 'n', 5) => Some(b"\x1b[0n".to_vec()),
            (None, 'n', 6) => {
                let (r, c) = s.cursor_position();
                Some(format!("\x1b[{};{}R", r + 1, c + 1).into_bytes())
            }
            (None, 'c', 0) => Some(b"\x1b[?1;2c".to_vec()),
            (Some(b'>'), 'c', _) => Some(b"\x1b[>0;1;0c".to_vec()),
            _ => None,
        };
        if let Some(r) = reply {
            self.bytes.push(r);
        }
    }
    fn unhandled_osc(&mut self, _: &mut vt100::Screen, p: &[&[u8]]) {
        if self.bytes.len() >= 16 {
            return;
        }
        if p.get(1) == Some(&b"?".as_slice()) {
            let color = match p.first().copied() {
                Some(b"10") => "10;rgb:dddd/dddd/dddd",
                Some(b"11") => "11;rgb:1111/1111/1111",
                _ => return,
            };
            self.bytes.push(format!("\x1b]{color}\x1b\\").into_bytes());
        }
    }
}
pub struct Output {
    bytes: VecDeque<u8>,
    start: u64,
    end: u64,
    capacity: usize,
    parser: vt100::Parser<Responses>,
}
#[derive(Debug, Serialize)]
pub struct Gap {
    pub from: u64,
    pub to: u64,
}
#[derive(Debug, Serialize)]
pub struct Stream {
    pub data_base64: String,
    pub next_cursor: u64,
    pub oldest_cursor: u64,
    pub end_cursor: u64,
    pub gap: Option<Gap>,
    pub has_more: bool,
}
#[derive(Debug, Serialize)]
pub struct Screen {
    pub version: String,
    pub rows: u16,
    pub cols: u16,
    pub cursor: (u16, u16),
    pub cursor_visible: bool,
    pub alternate_screen: bool,
    pub start_row: u16,
    pub lines: Vec<String>,
    pub next_row: Option<u16>,
}
impl Output {
    pub fn new(rows: u16, cols: u16, capacity: usize) -> Self {
        Self {
            bytes: VecDeque::new(),
            start: 0,
            end: 0,
            capacity,
            parser: vt100::Parser::new_with_callbacks(
                rows.try_into().unwrap(),
                cols.try_into().unwrap(),
                100,
                Responses::default(),
            ),
        }
    }
    pub fn push(&mut self, b: &[u8]) -> Vec<Vec<u8>> {
        self.parser.process(b);
        self.end += b.len() as u64;
        if b.len() >= self.capacity {
            self.bytes.clear();
            self.bytes.extend(&b[b.len() - self.capacity..]);
        } else {
            let remove = (self.bytes.len() + b.len()).saturating_sub(self.capacity);
            self.bytes.drain(..remove);
            self.bytes.extend(b);
        }
        self.start = self.end - self.bytes.len() as u64;
        std::mem::take(&mut self.parser.callbacks_mut().bytes)
    }
    pub fn resize(&mut self, rows: u16, cols: u16) {
        self.parser
            .set_size(rows.try_into().unwrap(), cols.try_into().unwrap());
    }
    pub fn end(&self) -> u64 {
        self.end
    }
    pub fn bracketed_paste(&self) -> bool {
        self.parser.screen().bracketed_paste()
    }
    pub fn application_cursor(&self) -> bool {
        self.parser.screen().application_cursor()
    }
    pub fn stream(&self, cursor: u64, limit: usize) -> Result<Stream> {
        if cursor > self.end {
            return Err(Failure::new(
                Code::CursorInvalid,
                "Cursor is ahead of terminal output",
                "Use a cursor returned by this terminal, or 0 to read available history.",
            ));
        }
        let actual = cursor.max(self.start);
        let b: Vec<_> = self
            .bytes
            .iter()
            .skip((actual - self.start) as usize)
            .take(limit)
            .copied()
            .collect();
        let next = actual + b.len() as u64;
        Ok(Stream {
            data_base64: base64::engine::general_purpose::STANDARD.encode(b),
            next_cursor: next,
            oldest_cursor: self.start,
            end_cursor: self.end,
            gap: (cursor < self.start).then_some(Gap {
                from: cursor,
                to: self.start,
            }),
            has_more: next < self.end,
        })
    }
    pub fn screen(
        &self,
        since: Option<&str>,
        start_row: u16,
        max_rows: u16,
    ) -> Result<Option<Screen>> {
        let s = self.parser.screen();
        let (rows, cols) = s.size();
        if start_row >= rows.get() {
            return Err(Failure::invalid("start_row exceeds terminal height"));
        }
        let cursor = s.cursor_position();
        let text = s.contents();
        let version = hash(
            format!(
                "{}:{}:{cursor:?}:{}:{}:{text}",
                rows,
                cols,
                s.alternate_screen(),
                s.hide_cursor()
            )
            .as_bytes(),
        );
        if since == Some(&version) {
            return Ok(None);
        }
        let all = s.rows(0, cols.get()).collect::<Vec<_>>();
        let mut lines = Vec::new();
        let mut budget = 32000usize;
        let mut next = start_row;
        for line in all
            .into_iter()
            .skip(start_row as usize)
            .take(max_rows as usize)
        {
            if line.len() > budget {
                break;
            }
            budget -= line.len();
            lines.push(line);
            next += 1;
        }
        Ok(Some(Screen {
            version,
            rows: rows.get(),
            cols: cols.get(),
            cursor,
            cursor_visible: !s.hide_cursor(),
            alternate_screen: s.alternate_screen(),
            start_row,
            lines,
            next_row: (next < rows.get()).then_some(next),
        }))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unicode_cursor_alternate_screen_and_overflow_are_explicit() {
        let mut o = Output::new(10, 40, 32);
        for b in "東京🙂".as_bytes().chunks(1) {
            o.push(b);
        }
        let s = o.screen(None, 0, 10).unwrap().unwrap();
        assert!(s.lines[0].contains("東京🙂"));
        assert!(o.screen(Some(&s.version), 0, 10).unwrap().is_none());
        o.push(b"\x1b[2J\x1b[H\x1b[?1049hTUI");
        assert!(o.screen(None, 0, 10).unwrap().unwrap().alternate_screen);
        o.push(b"\x1b[?1049l");
        assert!(!o.screen(None, 0, 10).unwrap().unwrap().alternate_screen);
        o.push(&[b'x'; 100]);
        let r = o.stream(0, 12).unwrap();
        assert!(r.gap.is_some());
        assert!(r.has_more);
        assert!(o.stream(9999, 12).is_err());
    }
}

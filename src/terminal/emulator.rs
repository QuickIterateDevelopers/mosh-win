// SPDX-License-Identifier: GPL-3.0-or-later
// mosh-win: Copyright 2026 the mosh-win authors.
// Ported from mosh (https://mosh.org): Copyright 2012 Keith Winstein and the mosh authors.

//! Terminal emulator: applies escape-sequence output to a Framebuffer
//! (port of mosh's terminal.cc, terminalfunctions.cc and terminaldispatcher.cc,
//! with the `vte` crate standing in for mosh's DEC/ANSI parser).

use unicode_width::UnicodeWidthChar;

use super::framebuffer::{Framebuffer, Hyperlink, Renditions};

const MAXIMUM_CLIPBOARD_SIZE: usize = 16 * 1024;

/// wcwidth(): -1 unprintable, 0 combining, 1 or 2 columns.
pub fn wcwidth(ch: char) -> i32 {
    if ch == '\0' {
        return -1;
    }
    match ch.width() {
        None => -1,
        Some(w) => w as i32,
    }
}

/// mosh's width rule: printing ISO 8859-1 is a cheap narrow shortcut.
fn char_width(ch: char) -> i32 {
    if ch == '\0' {
        -1
    } else if super::framebuffer::Cell::isprint_iso8859_1(ch) {
        1
    } else {
        wcwidth(ch)
    }
}

/// Feed `bytes` to the framebuffer. Each call uses a fresh parser:
/// the host sends complete sequences in every update.
pub fn act(fb: &mut Framebuffer, bytes: &[u8]) {
    let mut parser = vte::Parser::new();
    let mut p = Performer { fb };
    for &b in bytes {
        parser.advance(&mut p, b);
    }
}

struct Performer<'a> {
    fb: &'a mut Framebuffer,
}

struct Params(Vec<i64>);

impl Params {
    fn from_vte(params: &vte::Params) -> Self {
        let mut v: Vec<i64> = params.iter().map(|p| p.first().copied().unwrap_or(0) as i64).collect();
        if v.is_empty() {
            v.push(0);
        }
        Params(v)
    }

    /// Like mosh: missing or < 1 means default.
    fn get(&self, n: usize, default: i32) -> i32 {
        let mut ret = default as i64;
        if let Some(&v) = self.0.get(n) {
            ret = v;
        }
        if ret < 1 {
            ret = default as i64;
        }
        ret as i32
    }

    fn count(&self) -> usize {
        self.0.len()
    }
}

impl Performer<'_> {
    fn print_char(&mut self, ch: char) {
        let fb = &mut *self.fb;
        let chwidth = char_width(ch);
        match chwidth {
            1 | 2 => {
                if fb.ds.auto_wrap_mode && fb.ds.next_print_will_wrap {
                    fb.get_mutable_row(-1).set_wrap(true);
                    fb.ds.move_col(0, false, false);
                    fb.move_rows_autoscroll(1);
                } else if fb.ds.auto_wrap_mode && chwidth == 2 && fb.ds.cursor_col() == fb.ds.width() - 1 {
                    /* wrap 2-cell chars if no room, even without will-wrap flag */
                    fb.reset_cell_at(-1, -1);
                    fb.get_mutable_row(-1).set_wrap(false);
                    fb.ds.move_col(0, false, false);
                    fb.move_rows_autoscroll(1);
                }
                if fb.ds.insert_mode {
                    for _ in 0..chwidth {
                        let (r, c) = (fb.ds.cursor_row(), fb.ds.cursor_col());
                        fb.insert_cell(r, c);
                    }
                }
                let renditions = *fb.ds.renditions();
                let hyperlink = fb.ds.hyperlink().clone();
                let bg = fb.ds.background_rendition();
                {
                    let cell = fb.get_mutable_cell(-1, -1);
                    cell.reset(bg);
                    cell.append(ch);
                    cell.set_wide(chwidth == 2);
                    cell.set_renditions(renditions);
                    cell.set_hyperlink(hyperlink);
                }
                if chwidth == 2 && fb.ds.cursor_col() + 1 < fb.ds.width() {
                    /* erase overlapped cell */
                    let (r, c) = (fb.ds.cursor_row(), fb.ds.cursor_col() + 1);
                    fb.reset_cell_at(r, c);
                }
                fb.ds.move_col(chwidth, true, true);
            }
            0 => {
                /* combining character */
                let Some(cell) = fb.get_combining_cell() else { return };
                let mut advance = false;
                if cell.empty() {
                    /* cell starts with combining character */
                    cell.set_fallback(true);
                    advance = true;
                }
                if !cell.full() {
                    cell.append(ch);
                }
                if advance {
                    fb.ds.move_col(1, true, true);
                }
            }
            _ => {}
        }
    }

    fn control(&mut self, byte: u8) {
        let fb = &mut *self.fb;
        // HT is the only control function that preserves the wrap state.
        if byte != 0x09 {
            fb.ds.next_print_will_wrap = false;
        }
        match byte {
            0x0a | 0x0b | 0x0c | 0x84 => fb.move_rows_autoscroll(1),
            0x0d => fb.ds.move_col(0, false, false),
            0x08 => fb.ds.move_col(-1, true, false),
            0x8d => fb.move_rows_autoscroll(-1),
            0x85 => {
                fb.ds.move_col(0, false, false);
                fb.move_rows_autoscroll(1);
            }
            0x09 => ht_n(fb, 1),
            0x88 => fb.ds.set_tab(),
            0x07 => fb.ring_bell(),
            _ => {}
        }
    }

    fn csi(&mut self, key: &str, p: &Params) {
        let fb = &mut *self.fb;
        // functions registered with clears_wrap_state = false
        let preserves_wrap = matches!(key, "I" | "Z" | "g" | "?h" | "?l" | "m");
        if !preserves_wrap {
            fb.ds.next_print_will_wrap = false;
        }
        match key {
            "K" => match p.get(0, 0) {
                0 => {
                    let (c, w) = (fb.ds.cursor_col(), fb.ds.width());
                    clearline(fb, -1, c, w - 1);
                }
                1 => {
                    let c = fb.ds.cursor_col();
                    clearline(fb, -1, 0, c);
                }
                2 => fb.reset_row(-1),
                _ => {}
            },
            "J" => match p.get(0, 0) {
                0 => {
                    let (c, w) = (fb.ds.cursor_col(), fb.ds.width());
                    clearline(fb, -1, c, w - 1);
                    for y in fb.ds.cursor_row() + 1..fb.ds.height() {
                        fb.reset_row(y);
                    }
                }
                1 => {
                    for y in 0..fb.ds.cursor_row() {
                        fb.reset_row(y);
                    }
                    let c = fb.ds.cursor_col();
                    clearline(fb, -1, 0, c);
                }
                2 => {
                    for y in 0..fb.ds.height() {
                        fb.reset_row(y);
                    }
                }
                _ => {}
            },
            "A" => fb.ds.move_row(-p.get(0, 1), true),
            "B" => fb.ds.move_row(p.get(0, 1), true),
            "C" => fb.ds.move_col(p.get(0, 1), true, false),
            "D" => fb.ds.move_col(-p.get(0, 1), true, false),
            "H" | "f" => {
                fb.ds.move_row(p.get(0, 1) - 1, false);
                fb.ds.move_col(p.get(1, 1) - 1, false, false);
            }
            "I" | "Z" => {
                let mut param = p.get(0, 1);
                if key == "Z" {
                    param = -param;
                }
                if param != 0 {
                    ht_n(fb, param);
                }
            }
            "g" => match p.get(0, 0) {
                0 => {
                    let c = fb.ds.cursor_col();
                    fb.ds.clear_tab(c);
                }
                3 => {
                    fb.ds.clear_default_tabs();
                    for x in 0..fb.ds.width() {
                        fb.ds.clear_tab(x);
                    }
                }
                _ => {}
            },
            "?h" | "?l" => {
                let value = key == "?h";
                for i in 0..p.count() {
                    let param = p.get(i, 0);
                    if param == 9 || (1000..=1003).contains(&param) {
                        fb.ds.mouse_reporting_mode = if value { param } else { 0 };
                    } else if param == 1005 || param == 1006 || param == 1015 {
                        fb.ds.mouse_encoding_mode = if value { param } else { 0 };
                    } else {
                        set_dec_mode(fb, param, value);
                    }
                }
            }
            "h" | "l" => {
                for i in 0..p.count() {
                    if p.get(i, 0) == 4 {
                        fb.ds.insert_mode = key == "h";
                    }
                }
            }
            "r" => {
                let top = p.get(0, 1);
                let bottom = p.get(1, fb.ds.height());
                if bottom <= top || top > fb.ds.height() || (top == 0 && bottom == 1) {
                    return; /* invalid, xterm ignores */
                }
                fb.ds.set_scrolling_region(top - 1, bottom - 1);
                fb.ds.move_row(0, false);
                fb.ds.move_col(0, false, false);
            }
            "m" => {
                let mut i = 0;
                while i < p.count() {
                    let rendition = p.get(i, 0);
                    if (rendition == 38 || rendition == 48) && p.count() - i >= 3 && p.get(i + 1, -1) == 5 {
                        if rendition == 38 {
                            fb.ds.set_foreground_color(p.get(i + 2, 0));
                        } else {
                            fb.ds.set_background_color(p.get(i + 2, 0));
                        }
                        i += 3;
                        continue;
                    }
                    if (rendition == 38 || rendition == 48) && p.count() - i >= 5 && p.get(i + 1, -1) == 2 {
                        let color = Renditions::make_true_color(
                            p.get(i + 2, 0) as u32,
                            p.get(i + 3, 0) as u32,
                            p.get(i + 4, 0) as u32,
                        ) as i32;
                        if rendition == 38 {
                            fb.ds.set_foreground_color(color);
                        } else {
                            fb.ds.set_background_color(color);
                        }
                        i += 5;
                        continue;
                    }
                    fb.ds.add_rendition(rendition);
                    i += 1;
                }
            }
            "L" => {
                let r = fb.ds.cursor_row();
                fb.insert_line(r, p.get(0, 1));
                fb.ds.move_col(0, false, false);
            }
            "M" => {
                let r = fb.ds.cursor_row();
                fb.delete_line(r, p.get(0, 1));
                fb.ds.move_col(0, false, false);
            }
            "@" => {
                for _ in 0..p.get(0, 1) {
                    let (r, c) = (fb.ds.cursor_row(), fb.ds.cursor_col());
                    fb.insert_cell(r, c);
                }
            }
            "P" => {
                for _ in 0..p.get(0, 1) {
                    let (r, c) = (fb.ds.cursor_row(), fb.ds.cursor_col());
                    fb.delete_cell(r, c);
                }
            }
            "d" => fb.ds.move_row(p.get(0, 1) - 1, false),
            "G" | "`" => fb.ds.move_col(p.get(0, 1) - 1, false, false),
            "X" => {
                let num = p.get(0, 1);
                let c = fb.ds.cursor_col();
                let limit = (c + num - 1).min(fb.ds.width() - 1);
                clearline(fb, -1, c, limit);
            }
            "!p" => fb.soft_reset(),
            "S" => fb.scroll(p.get(0, 1)),
            "T" => fb.scroll(-p.get(0, 1)),
            // c, >c, n: replies to the host are irrelevant on the client
            _ => {}
        }
    }

    fn esc(&mut self, key: &str) {
        let fb = &mut *self.fb;
        fb.ds.next_print_will_wrap = false;
        match key {
            "7" => fb.ds.save_cursor(),
            "8" => fb.ds.restore_cursor(),
            "c" => fb.reset(),
            "#8" => {
                for y in 0..fb.ds.height() {
                    for x in 0..fb.ds.width() {
                        fb.reset_cell_at(y, x);
                        fb.get_mutable_cell(y, x).append('E');
                    }
                }
            }
            _ => {}
        }
    }

    fn osc(&mut self, s: Vec<char>) {
        let fb = &mut *self.fb;
        if s.len() >= 5 && s[0] == '5' && s[1] == '2' && s[2] == ';' && s[3] == 'c' && s[4] == ';' {
            fb.set_clipboard(s[5..].to_vec());
            return;
        }
        if s.is_empty() {
            return;
        }
        let (cmd_num, offset) = if s[0] == ';' {
            (0i64, 1usize)
        } else if s.len() >= 2 && s[1] == ';' {
            (s[0] as i64 - '0' as i64, 2usize)
        } else {
            (-1, 0)
        };
        if cmd_num == 8 {
            if s.iter().any(|&c| (c as u32) < 32 || (c as u32) > 126) {
                return;
            }
            let st: String = s.iter().collect();
            if st.len() <= 2 {
                return;
            }
            let Some(second) = st[2..].find(';').map(|i| i + 2) else { return };
            let url = st[second + 1..].to_string();
            fb.ds.set_hyperlink(Hyperlink::new(st[2..second].to_string(), url));
            return;
        }
        let set_icon = cmd_num == 0 || cmd_num == 1;
        let set_title = cmd_num == 0 || cmd_num == 2;
        if set_icon || set_title {
            fb.set_title_initialized();
            let title_length = s.len().min(256);
            let newtitle: Vec<char> = if offset <= title_length { s[offset..title_length].to_vec() } else { Vec::new() };
            if set_icon {
                fb.set_icon_name(newtitle.clone());
            }
            if set_title {
                fb.set_window_title(newtitle);
            }
        }
    }
}

fn clearline(fb: &mut Framebuffer, row: i32, start: i32, end: i32) {
    for col in start..=end {
        fb.reset_cell_at(row, col);
    }
}

fn ht_n(fb: &mut Framebuffer, count: i32) {
    let mut col = fb.ds.get_next_tab(count);
    if col == -1 {
        col = fb.ds.width() - 1;
    }
    /* A horizontal tab is the only operation that preserves but does not set the wrap state. */
    let save = fb.ds.next_print_will_wrap;
    fb.ds.move_col(col, false, false);
    fb.ds.next_print_will_wrap = save;
}

fn set_dec_mode(fb: &mut Framebuffer, param: i32, value: bool) {
    match param {
        1 => fb.ds.application_mode_cursor_keys = value,
        3 => {
            /* 80/132. Ignore but clear screen. */
            fb.ds.move_row(0, false);
            fb.ds.move_col(0, false, false);
            for y in 0..fb.ds.height() {
                fb.reset_row(y);
            }
        }
        5 => fb.ds.reverse_video = value,
        6 => {
            fb.ds.move_row(0, false);
            fb.ds.move_col(0, false, false);
            fb.ds.origin_mode = value;
        }
        7 => fb.ds.auto_wrap_mode = value,
        25 => fb.ds.cursor_visible = value,
        1004 => fb.ds.mouse_focus_event = value,
        1007 => fb.ds.mouse_alternate_scroll = value,
        2004 => fb.ds.bracketed_paste = value,
        _ => {}
    }
}

impl vte::Perform for Performer<'_> {
    fn print(&mut self, c: char) {
        self.print_char(c);
    }

    fn execute(&mut self, byte: u8) {
        self.control(byte);
    }

    fn csi_dispatch(&mut self, params: &vte::Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        let mut key: String = intermediates.iter().map(|&b| b as char).collect();
        key.push(action);
        self.csi(&key, &Params::from_vte(params));
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if ignore {
            return;
        }
        /* handle 7-bit ESC-encoding of C1 control characters */
        if intermediates.is_empty() && (0x40..=0x5f).contains(&byte) {
            self.control(byte + 0x40);
            return;
        }
        let mut key: String = intermediates.iter().map(|&b| b as char).collect();
        key.push(byte as char);
        self.esc(&key);
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        let joined = params.join(&b';');
        let s: Vec<char> = String::from_utf8_lossy(&joined).chars().take(MAXIMUM_CLIPBOARD_SIZE).collect();
        self.osc(s);
    }
}

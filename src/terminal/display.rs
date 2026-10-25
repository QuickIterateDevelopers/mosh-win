// SPDX-License-Identifier: GPL-3.0-or-later
// mosh-win: Copyright 2026 the mosh-win authors.
// Ported from mosh (https://mosh.org): Copyright 2012 Keith Winstein and the mosh authors.

//! Produce the escape sequences that turn one framebuffer into another
//! (port of mosh's terminaldisplay.cc). The client uses this to draw the
//! synchronized screen onto the local terminal.

use std::rc::Rc;

use super::framebuffer::{Cell, Framebuffer, Hyperlink, MOUSE_ENCODING_DEFAULT, MOUSE_REPORTING_NONE, Renditions, Row};

pub struct Display {
    pub has_ech: bool,
    pub has_bce: bool,
    pub has_title: bool,
    pub smcup: &'static str,
    pub rmcup: &'static str,
}

fn initial_rendition() -> Renditions {
    Renditions::new(0)
}

struct FrameState<'a> {
    str: String,
    cursor_x: i32,
    cursor_y: i32,
    current_rendition: Renditions,
    current_hyperlink: Hyperlink,
    cursor_visible: bool,
    last_frame: &'a Framebuffer,
}

impl<'a> FrameState<'a> {
    fn new(last: &'a Framebuffer) -> Self {
        FrameState {
            str: String::with_capacity((last.ds.width() * last.ds.height() * 4) as usize),
            cursor_x: 0,
            cursor_y: 0,
            current_rendition: Renditions::new(0),
            current_hyperlink: Hyperlink::default(),
            cursor_visible: last.ds.cursor_visible,
            last_frame: last,
        }
    }

    fn append(&mut self, s: &str) {
        self.str.push_str(s);
    }

    fn append_n(&mut self, n: usize, c: char) {
        for _ in 0..n {
            self.str.push(c);
        }
    }

    fn append_cell(&mut self, cell: &Cell) {
        cell.print_grapheme(&mut self.str);
    }

    fn append_silent_move(&mut self, y: i32, x: i32) {
        if self.cursor_x == x && self.cursor_y == y {
            return;
        }
        /* turn off cursor if necessary before moving cursor */
        if self.cursor_visible {
            self.append("\x1b[?25l");
            self.cursor_visible = false;
        }
        self.append_move(y, x);
    }

    fn append_move(&mut self, y: i32, x: i32) {
        let last_x = self.cursor_x;
        let last_y = self.cursor_y;
        self.cursor_x = x;
        self.cursor_y = y;
        // Only optimize if cursor pos is known
        if last_x != -1 && last_y != -1 {
            // Can we use CR and/or LF?  They're cheap and easier to trace.
            if x == 0 && y - last_y >= 0 && y - last_y < 5 {
                if last_x != 0 {
                    self.str.push('\r');
                }
                self.append_n((y - last_y) as usize, '\n');
                return;
            }
            // Backspaces are good too.
            if y == last_y && x - last_x < 0 && x - last_x > -5 {
                self.append_n((last_x - x) as usize, '\x08');
                return;
            }
        }
        self.append(&format!("\x1b[{};{}H", y + 1, x + 1));
    }

    fn update_rendition(&mut self, r: &Renditions, force: bool) {
        if force || self.current_rendition != *r {
            self.str.push_str(&r.sgr());
            self.current_rendition = *r;
        }
    }

    fn update_hyperlink(&mut self, h: &Hyperlink, force: bool) {
        if force || self.current_hyperlink != *h {
            self.str.push_str(&h.osc8());
            self.current_hyperlink = h.clone();
        }
    }
}

impl Display {
    pub fn new() -> Self {
        Display { has_ech: true, has_bce: true, has_title: true, smcup: "\x1b[?1049h", rmcup: "\x1b[?1049l" }
    }

    pub fn open(&self) -> String {
        format!("{}\x1b[?1h", self.smcup)
    }

    pub fn close(&self) -> String {
        format!(
            "\x1b[?1l\x1b[0m\x1b[?25h\x1b[?1003l\x1b[?1002l\x1b[?1001l\x1b[?1000l\x1b[?1015l\x1b[?1006l\x1b[?1005l{}",
            self.rmcup
        )
    }

    fn can_use_erase(&self, frame: &FrameState) -> bool {
        self.has_bce || (frame.current_rendition == initial_rendition() && frame.current_hyperlink.is_empty())
    }

    pub fn new_frame(&self, mut initialized: bool, last: &Framebuffer, f: &Framebuffer) -> String {
        let mut frame = FrameState::new(last);

        /* has bell been rung? */
        if f.bell_count() != frame.last_frame.bell_count() {
            frame.str.push('\x07');
        }

        /* has icon name or window title changed? */
        if self.has_title
            && f.is_title_initialized()
            && (!initialized
                || f.icon_name() != frame.last_frame.icon_name()
                || f.window_title() != frame.last_frame.window_title())
        {
            if f.icon_name() == f.window_title() {
                frame.append("\x1b]0;");
                frame.str.extend(f.window_title().iter());
                frame.str.push('\x07');
            } else {
                frame.append("\x1b]1;");
                frame.str.extend(f.icon_name().iter());
                frame.str.push('\x07');
                frame.append("\x1b]2;");
                frame.str.extend(f.window_title().iter());
                frame.str.push('\x07');
            }
        }

        /* has clipboard changed? */
        if f.clipboard() != frame.last_frame.clipboard() {
            frame.append("\x1b]52;c;");
            frame.str.extend(f.clipboard().iter());
            frame.str.push('\x07');
        }

        /* has reverse video state changed? */
        if !initialized || f.ds.reverse_video != frame.last_frame.ds.reverse_video {
            frame.append(if f.ds.reverse_video { "\x1b[?5h" } else { "\x1b[?5l" });
        }

        /* has size changed? */
        if !initialized
            || f.ds.width() != frame.last_frame.ds.width()
            || f.ds.height() != frame.last_frame.ds.height()
        {
            frame.append("\x1b[r");
            frame.append("\x1b[0m\x1b[H\x1b[2J");
            initialized = false;
            frame.cursor_x = 0;
            frame.cursor_y = 0;
            frame.current_rendition = initial_rendition();
            frame.current_hyperlink = Hyperlink::default();
        } else {
            frame.cursor_x = frame.last_frame.ds.cursor_col();
            frame.cursor_y = frame.last_frame.ds.cursor_row();
            frame.current_rendition = *frame.last_frame.ds.renditions();
            frame.current_hyperlink = frame.last_frame.ds.hyperlink().clone();
        }

        /* is cursor visibility initialized? */
        if !initialized {
            frame.cursor_visible = false;
            frame.append("\x1b[?25l");
        }

        let mut frame_y = 0;
        let mut blank_row: Option<Rc<Row>> = None;
        let mut rows: Vec<Rc<Row>> = frame.last_frame.rows().clone();

        /* Extend rows if we've gotten a resize and new is wider than old */
        if frame.last_frame.ds.width() < f.ds.width() {
            for p in rows.iter_mut() {
                let mut r = (**p).clone();
                r.cells.resize(f.ds.width() as usize, Cell::new(f.ds.background_rendition()));
                *p = Rc::new(r);
            }
        }

        /* Add rows if we've gotten a resize and new is taller than old */
        if (rows.len() as i32) < f.ds.height() {
            let br = Rc::new(Row::new(f.ds.width() as usize, 0));
            rows.resize(f.ds.height() as usize, br.clone());
            blank_row = Some(br);
        }

        /* shortcut -- has display moved up by a certain number of lines? */
        if initialized {
            let mut lines_scrolled = 0;
            let mut scroll_height = 0;

            for row in 0..f.ds.height() {
                let new_row = f.row_rc(0);
                let old_row = &rows[row as usize];
                if !(Rc::ptr_eq(new_row, old_row) || **new_row == **old_row) {
                    continue;
                }
                /* if row 0, we're looking at ourselves and probably didn't scroll */
                if row == 0 {
                    break;
                }
                /* found a scroll */
                lines_scrolled = row;
                scroll_height = 1;

                /* how big is the region that was scrolled? */
                let mut region_height = 1;
                while lines_scrolled + region_height < f.ds.height() {
                    if *f.get_row(region_height) == *rows[(lines_scrolled + region_height) as usize] {
                        scroll_height = region_height + 1;
                    } else {
                        break;
                    }
                    region_height += 1;
                }
                break;
            }

            if scroll_height != 0 {
                frame_y = scroll_height;

                if lines_scrolled != 0 {
                    let br = blank_row
                        .get_or_insert_with(|| Rc::new(Row::new(f.ds.width() as usize, 0)))
                        .clone();

                    frame.update_rendition(&initial_rendition(), true);
                    frame.update_hyperlink(&Hyperlink::default(), true);

                    let top_margin = 0;
                    let bottom_margin = top_margin + lines_scrolled + scroll_height - 1;

                    /* Common case: if we're already on the bottom line and we're scrolling
                     * the whole screen, just do a CR and LFs. */
                    if scroll_height + lines_scrolled == f.ds.height() && frame.cursor_y + 1 == f.ds.height() {
                        frame.str.push('\r');
                        frame.append_n(lines_scrolled as usize, '\n');
                        frame.cursor_x = 0;
                    } else {
                        /* set scrolling region */
                        frame.append(&format!("\x1b[{};{}r", top_margin + 1, bottom_margin + 1));
                        /* go to bottom of scrolling region */
                        frame.cursor_x = -1;
                        frame.cursor_y = -1;
                        frame.append_silent_move(bottom_margin, 0);
                        /* scroll */
                        frame.append_n(lines_scrolled as usize, '\n');
                        /* reset scrolling region */
                        frame.append("\x1b[r");
                        /* invalidate cursor position after unsetting scrolling region */
                        frame.cursor_x = -1;
                        frame.cursor_y = -1;
                    }

                    /* do the move in our local index */
                    for i in top_margin..=bottom_margin {
                        if i + lines_scrolled <= bottom_margin {
                            rows[i as usize] = rows[(i + lines_scrolled) as usize].clone();
                        } else {
                            rows[i as usize] = br.clone();
                        }
                    }
                }
            }
        }

        /* Now update the display, row by row */
        let mut wrap = false;
        while frame_y < f.ds.height() {
            let old_row = rows[frame_y as usize].clone();
            wrap = self.put_row(initialized, &mut frame, f, frame_y, &old_row, wrap);
            frame_y += 1;
        }

        /* has cursor location changed? */
        if !initialized || f.ds.cursor_row() != frame.cursor_y || f.ds.cursor_col() != frame.cursor_x {
            frame.append_move(f.ds.cursor_row(), f.ds.cursor_col());
        }

        /* has cursor visibility changed? */
        if !initialized || f.ds.cursor_visible != frame.cursor_visible {
            frame.append(if f.ds.cursor_visible { "\x1b[?25h" } else { "\x1b[?25l" });
        }

        /* have renditions changed? */
        frame.update_rendition(f.ds.renditions(), !initialized);

        /* has hyperlink changed? */
        frame.update_hyperlink(f.ds.hyperlink(), !initialized);

        /* has bracketed paste mode changed? */
        if !initialized || f.ds.bracketed_paste != frame.last_frame.ds.bracketed_paste {
            frame.append(if f.ds.bracketed_paste { "\x1b[?2004h" } else { "\x1b[?2004l" });
        }

        /* has mouse reporting mode changed? */
        if !initialized || f.ds.mouse_reporting_mode != frame.last_frame.ds.mouse_reporting_mode {
            if f.ds.mouse_reporting_mode == MOUSE_REPORTING_NONE {
                frame.append("\x1b[?1003l\x1b[?1002l\x1b[?1001l\x1b[?1000l");
            } else {
                if frame.last_frame.ds.mouse_reporting_mode != MOUSE_REPORTING_NONE {
                    let s = format!("\x1b[?{}l", frame.last_frame.ds.mouse_reporting_mode);
                    frame.append(&s);
                }
                frame.append(&format!("\x1b[?{}h", f.ds.mouse_reporting_mode));
            }
        }

        /* has mouse focus mode changed? */
        if !initialized || f.ds.mouse_focus_event != frame.last_frame.ds.mouse_focus_event {
            frame.append(if f.ds.mouse_focus_event { "\x1b[?1004h" } else { "\x1b[?1004l" });
        }

        /* has mouse encoding mode changed? */
        if !initialized || f.ds.mouse_encoding_mode != frame.last_frame.ds.mouse_encoding_mode {
            if f.ds.mouse_encoding_mode == MOUSE_ENCODING_DEFAULT {
                frame.append("\x1b[?1015l\x1b[?1006l\x1b[?1005l");
            } else {
                if frame.last_frame.ds.mouse_encoding_mode != MOUSE_ENCODING_DEFAULT {
                    let s = format!("\x1b[?{}l", frame.last_frame.ds.mouse_encoding_mode);
                    frame.append(&s);
                }
                frame.append(&format!("\x1b[?{}h", f.ds.mouse_encoding_mode));
            }
        }

        frame.str
    }

    fn put_row(
        &self,
        initialized: bool,
        frame: &mut FrameState,
        f: &Framebuffer,
        frame_y: i32,
        old_row: &Rc<Row>,
        wrap: bool,
    ) -> bool {
        let mut frame_x = 0;
        let row_rc = f.row_rc(frame_y);
        let cells = &row_rc.cells;
        let old_cells = &old_row.cells;

        /* If we're forced to write the first column because of wrap, go ahead and do so. */
        if wrap {
            let cell = &cells[0];
            frame.update_rendition(cell.renditions(), false);
            frame.update_hyperlink(cell.hyperlink(), false);
            frame.append_cell(cell);
            frame_x += cell.width();
            frame.cursor_x += cell.width();
        }

        /* If rows are the same object, we don't need to do anything at all. */
        if initialized && Rc::ptr_eq(row_rc, old_row) {
            return false;
        }

        let wrap_this = row_rc.get_wrap();
        let row_width = f.ds.width();
        let mut clear_count = 0;
        let mut wrote_last_cell = false;
        let mut blank_renditions = initial_rendition();
        let mut blank_hyperlink = Hyperlink::default();

        /* iterate for every cell */
        while frame_x < row_width {
            let cell = &cells[frame_x as usize];

            /* Does cell need to be drawn?  Skip all this. */
            if initialized && clear_count == 0 && old_cells.get(frame_x as usize) == Some(cell) {
                frame_x += cell.width();
                continue;
            }

            /* Slurp up all the empty cells */
            if cell.empty() {
                if clear_count == 0 {
                    blank_renditions = *cell.renditions();
                    blank_hyperlink = cell.hyperlink().clone();
                }
                if *cell.renditions() == blank_renditions && *cell.hyperlink() == blank_hyperlink {
                    /* Remember run of blank cells */
                    clear_count += 1;
                    frame_x += 1;
                    continue;
                }
            }

            /* Clear or write cells within the row (not to end). */
            if clear_count != 0 {
                /* Move to the right position. */
                frame.append_silent_move(frame_y, frame_x - clear_count);
                frame.update_rendition(&blank_renditions, false);
                frame.update_hyperlink(&blank_hyperlink, false);
                if self.can_use_erase(frame) && self.has_ech && clear_count > 4 {
                    frame.append(&format!("\x1b[{clear_count}X"));
                } else {
                    frame.append_n(clear_count as usize, ' ');
                    frame.cursor_x = frame_x;
                }
                clear_count = 0;
                // If the current character is *another* empty cell in a different rendition,
                // we restart counting and continue here
                if cell.empty() {
                    blank_renditions = *cell.renditions();
                    blank_hyperlink = cell.hyperlink().clone();
                    clear_count = 1;
                    frame_x += 1;
                    continue;
                }
            }

            /* Now draw a character cell. */
            let cell_width = cell.width();
            /* If we are about to print the last character in a wrapping row,
            trash the cursor position to force explicit positioning. */
            if wrap_this && frame_x + cell_width >= row_width {
                frame.cursor_x = -1;
                frame.cursor_y = -1;
            }
            frame.append_silent_move(frame_y, frame_x);
            frame.update_rendition(cell.renditions(), false);
            frame.update_hyperlink(cell.hyperlink(), false);
            frame.append_cell(cell);
            frame_x += cell_width;
            frame.cursor_x += cell_width;
            if frame_x >= row_width {
                wrote_last_cell = true;
            }
        }

        /* End of line. */
        /* Clear or write empty cells at EOL. */
        if clear_count != 0 {
            frame.append_silent_move(frame_y, frame_x - clear_count);
            frame.update_rendition(&blank_renditions, false);
            frame.update_hyperlink(&blank_hyperlink, false);
            if self.can_use_erase(frame) && !wrap_this {
                frame.append("\x1b[K");
            } else {
                frame.append_n(clear_count as usize, ' ');
                frame.cursor_x = frame_x;
                wrote_last_cell = true;
            }
        }

        if !(wrote_last_cell && frame_y < f.ds.height() - 1) {
            return false;
        }
        /* To hint that a word-select should group the end of one line
        with the beginning of the next, we let the real cursor
        actually wrap around in cases where it wrapped around for us. */
        if wrap_this {
            frame.cursor_x = 0;
            frame.cursor_y += 1;
            return true;
        }
        /* Resort to CR/LF and update our cursor. */
        frame.append("\r\n");
        frame.cursor_x = 0;
        frame.cursor_y += 1;
        false
    }
}

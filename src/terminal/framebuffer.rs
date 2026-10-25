// SPDX-License-Identifier: GPL-3.0-or-later
// mosh-win: Copyright 2026 the mosh-win authors.
// Ported from mosh (https://mosh.org): Copyright 2012 Keith Winstein and the mosh authors.

//! Terminal framebuffer (port of mosh's terminalframebuffer.{h,cc}).
//!
//! Rows are shared between framebuffers with `Rc` and copied on write, so
//! cloning a framebuffer is cheap and row identity can short-cut diffs.

use std::cell::Cell as StdCell;
use std::rc::Rc;

pub type Color = u32;

pub const BOLD: u8 = 0;
pub const FAINT: u8 = 1;
pub const ITALIC: u8 = 2;
pub const UNDERLINED: u8 = 3;
pub const BLINK: u8 = 4;
pub const INVERSE: u8 = 5;
pub const INVISIBLE: u8 = 6;

const TRUE_COLOR_MASK: u32 = 0x100_0000;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Renditions {
    foreground_color: u32,
    background_color: u32,
    attributes: u8,
}

impl Renditions {
    pub fn new(background: Color) -> Self {
        Renditions { foreground_color: 0, background_color: background, attributes: 0 }
    }

    pub fn make_true_color(r: u32, g: u32, b: u32) -> u32 {
        TRUE_COLOR_MASK | ((r & 0xff) << 16) | ((g & 0xff) << 8) | (b & 0xff)
    }

    fn is_true_color(c: u32) -> bool {
        c & TRUE_COLOR_MASK != 0
    }

    pub fn set_foreground_color(&mut self, num: i32) {
        if (0..=255).contains(&num) {
            self.foreground_color = 30 + num as u32;
        } else if Self::is_true_color(num as u32) {
            self.foreground_color = num as u32;
        }
    }

    pub fn set_background_color(&mut self, num: i32) {
        if (0..=255).contains(&num) {
            self.background_color = 40 + num as u32;
        } else if Self::is_true_color(num as u32) {
            self.background_color = num as u32;
        }
    }

    pub fn background_rendition(&self) -> Color {
        self.background_color
    }

    pub fn set_attribute(&mut self, attr: u8, val: bool) {
        if val {
            self.attributes |= 1 << attr;
        } else {
            self.attributes &= !(1 << attr);
        }
    }

    pub fn get_attribute(&self, attr: u8) -> bool {
        self.attributes & (1 << attr) != 0
    }

    /// This routine cannot be used to set a color beyond the 16-color set.
    pub fn set_rendition(&mut self, num: i32) {
        if num == 0 {
            self.attributes = 0;
            self.foreground_color = 0;
            self.background_color = 0;
            return;
        }
        match num {
            39 => return self.foreground_color = 0,
            49 => return self.background_color = 0,
            30..=37 => return self.foreground_color = num as u32,
            40..=47 => return self.background_color = num as u32,
            90..=97 => return self.foreground_color = (num - 90 + 38) as u32,
            100..=107 => return self.background_color = (num - 100 + 48) as u32,
            _ => {}
        }
        let value = num < 9;
        match num {
            1 | 22 => self.set_attribute(BOLD, value),
            3 | 23 => self.set_attribute(ITALIC, value),
            4 | 24 => self.set_attribute(UNDERLINED, value),
            5 | 25 => self.set_attribute(BLINK, value),
            7 | 27 => self.set_attribute(INVERSE, value),
            8 | 28 => self.set_attribute(INVISIBLE, value),
            _ => {}
        }
    }

    pub fn sgr(&self) -> String {
        let mut ret = String::from("\x1b[0");
        if self.get_attribute(BOLD) {
            ret.push_str(";1");
        }
        if self.get_attribute(ITALIC) {
            ret.push_str(";3");
        }
        if self.get_attribute(UNDERLINED) {
            ret.push_str(";4");
        }
        if self.get_attribute(BLINK) {
            ret.push_str(";5");
        }
        if self.get_attribute(INVERSE) {
            ret.push_str(";7");
        }
        if self.get_attribute(INVISIBLE) {
            ret.push_str(";8");
        }
        let fg = self.foreground_color;
        if fg != 0 {
            if Self::is_true_color(fg) {
                ret.push_str(&format!(";38;2;{};{};{}", (fg >> 16) & 0xff, (fg >> 8) & 0xff, fg & 0xff));
            } else if fg > 37 {
                ret.push_str(&format!(";38;5;{}", fg - 30));
            } else {
                ret.push_str(&format!(";{fg}"));
            }
        }
        let bg = self.background_color;
        if bg != 0 {
            if Self::is_true_color(bg) {
                ret.push_str(&format!(";48;2;{};{};{}", (bg >> 16) & 0xff, (bg >> 8) & 0xff, bg & 0xff));
            } else if bg > 47 {
                ret.push_str(&format!(";48;5;{}", bg - 40));
            } else {
                ret.push_str(&format!(";{bg}"));
            }
        }
        ret.push('m');
        ret
    }
}

#[derive(Clone, Default, Debug)]
pub struct Hyperlink {
    rep: Option<Rc<(String, String)>>, // (params, url)
}

impl Hyperlink {
    pub fn new(params: String, url: String) -> Self {
        if url.is_empty() { Hyperlink { rep: None } } else { Hyperlink { rep: Some(Rc::new((params, url))) } }
    }

    pub fn is_empty(&self) -> bool {
        self.rep.is_none()
    }

    pub fn osc8(&self) -> String {
        let mut ret = String::from("\x1b]8;");
        if let Some(r) = &self.rep {
            ret.push_str(&r.0);
        }
        ret.push(';');
        if let Some(r) = &self.rep {
            ret.push_str(&r.1);
        }
        ret.push_str("\x1b\\");
        ret
    }
}

impl PartialEq for Hyperlink {
    fn eq(&self, x: &Self) -> bool {
        match (&self.rep, &x.rep) {
            (None, None) => true,
            (Some(a), Some(b)) => Rc::ptr_eq(a, b) || (a.0 == b.0 && a.1 == b.1),
            _ => false,
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct Cell {
    contents: String,
    renditions: Renditions,
    hyperlink: Hyperlink,
    wide: bool,
    fallback: bool,
    wrap: bool,
}

impl Cell {
    pub fn new(background_color: Color) -> Self {
        Cell {
            contents: String::new(),
            renditions: Renditions::new(background_color),
            hyperlink: Hyperlink::default(),
            wide: false,
            fallback: false,
            wrap: false,
        }
    }

    pub fn reset(&mut self, background_color: Color) {
        *self = Cell::new(background_color);
    }

    pub fn empty(&self) -> bool {
        self.contents.is_empty()
    }

    /* 32 seems like a reasonable limit on combining characters */
    pub fn full(&self) -> bool {
        self.contents.len() >= 32
    }

    pub fn clear(&mut self) {
        self.contents.clear();
    }

    pub fn is_blank(&self) -> bool {
        self.contents.is_empty() || self.contents == " " || self.contents == "\u{a0}"
    }

    pub fn contents_match(&self, other: &Cell) -> bool {
        (self.is_blank() && other.is_blank()) || self.contents == other.contents
    }

    pub fn isprint_iso8859_1(c: char) -> bool {
        let c = c as u32;
        (0xa0..=0xff).contains(&c) || (0x20..=0x7e).contains(&c)
    }

    pub fn append(&mut self, c: char) {
        self.contents.push(c);
    }

    pub fn print_grapheme(&self, output: &mut String) {
        if self.contents.is_empty() {
            output.push(' ');
            return;
        }
        /* cells that begin with combining character get combiner attached to no-break space */
        if self.fallback {
            output.push('\u{a0}');
        }
        output.push_str(&self.contents);
    }

    pub fn contents(&self) -> &str {
        &self.contents
    }
    pub fn hyperlink(&self) -> &Hyperlink {
        &self.hyperlink
    }
    pub fn set_hyperlink(&mut self, l: Hyperlink) {
        self.hyperlink = l;
    }
    pub fn renditions(&self) -> &Renditions {
        &self.renditions
    }
    pub fn renditions_mut(&mut self) -> &mut Renditions {
        &mut self.renditions
    }
    pub fn set_renditions(&mut self, r: Renditions) {
        self.renditions = r;
    }
    pub fn wide(&self) -> bool {
        self.wide
    }
    pub fn set_wide(&mut self, w: bool) {
        self.wide = w;
    }
    pub fn width(&self) -> i32 {
        self.wide as i32 + 1
    }
    pub fn set_fallback(&mut self, f: bool) {
        self.fallback = f;
    }
    pub fn wrap(&self) -> bool {
        self.wrap
    }
    pub fn set_wrap(&mut self, f: bool) {
        self.wrap = f;
    }
}

thread_local! {
    static GEN_COUNTER: StdCell<u64> = const { StdCell::new(0) };
}

fn next_gen() -> u64 {
    GEN_COUNTER.with(|g| {
        let v = g.get();
        g.set(v + 1);
        v
    })
}

#[derive(Clone, Debug)]
pub struct Row {
    pub cells: Vec<Cell>,
    /// Generation counter, used to quickly rule out the possibility of two rows being identical.
    pub generation: u64,
}

impl PartialEq for Row {
    fn eq(&self, x: &Self) -> bool {
        self.generation == x.generation && self.cells == x.cells
    }
}

impl Row {
    pub fn new(width: usize, background_color: Color) -> Self {
        Row { cells: vec![Cell::new(background_color); width], generation: next_gen() }
    }

    pub fn insert_cell(&mut self, col: usize, background_color: Color) {
        self.cells.insert(col, Cell::new(background_color));
        self.cells.pop();
    }

    pub fn delete_cell(&mut self, col: usize, background_color: Color) {
        self.cells.push(Cell::new(background_color));
        self.cells.remove(col);
    }

    pub fn reset(&mut self, background_color: Color) {
        self.generation = next_gen();
        for c in &mut self.cells {
            c.reset(background_color);
        }
    }

    pub fn get_wrap(&self) -> bool {
        self.cells.last().unwrap().wrap()
    }

    pub fn set_wrap(&mut self, w: bool) {
        self.cells.last_mut().unwrap().set_wrap(w);
    }
}

#[derive(Clone, Debug)]
pub struct SavedCursor {
    pub cursor_col: i32,
    pub cursor_row: i32,
    pub renditions: Renditions,
    pub auto_wrap_mode: bool,
    pub origin_mode: bool,
}

impl Default for SavedCursor {
    fn default() -> Self {
        SavedCursor { cursor_col: 0, cursor_row: 0, renditions: Renditions::new(0), auto_wrap_mode: true, origin_mode: false }
    }
}

pub const MOUSE_REPORTING_NONE: i32 = 0;
pub const MOUSE_ENCODING_DEFAULT: i32 = 0;

#[derive(Clone, Debug)]
pub struct DrawState {
    width: i32,
    height: i32,
    cursor_col: i32,
    cursor_row: i32,
    combining_char_col: i32,
    combining_char_row: i32,
    default_tabs: bool,
    tabs: Vec<bool>,
    scrolling_region_top_row: i32,
    scrolling_region_bottom_row: i32,
    renditions: Renditions,
    hyperlink: Hyperlink,
    save: SavedCursor,

    pub next_print_will_wrap: bool,
    pub origin_mode: bool,
    pub auto_wrap_mode: bool,
    pub insert_mode: bool,
    pub cursor_visible: bool,
    pub reverse_video: bool,
    pub bracketed_paste: bool,
    pub mouse_reporting_mode: i32,
    pub mouse_focus_event: bool,
    pub mouse_alternate_scroll: bool,
    pub mouse_encoding_mode: i32,
    pub application_mode_cursor_keys: bool,
}

impl PartialEq for DrawState {
    /* only compare fields that affect display */
    fn eq(&self, x: &Self) -> bool {
        self.width == x.width
            && self.height == x.height
            && self.cursor_col == x.cursor_col
            && self.cursor_row == x.cursor_row
            && self.cursor_visible == x.cursor_visible
            && self.reverse_video == x.reverse_video
            && self.renditions == x.renditions
            && self.bracketed_paste == x.bracketed_paste
            && self.mouse_reporting_mode == x.mouse_reporting_mode
            && self.mouse_focus_event == x.mouse_focus_event
            && self.mouse_alternate_scroll == x.mouse_alternate_scroll
            && self.mouse_encoding_mode == x.mouse_encoding_mode
            && self.hyperlink == x.hyperlink
    }
}

impl DrawState {
    pub fn new(width: i32, height: i32) -> Self {
        let mut ds = DrawState {
            width,
            height,
            cursor_col: 0,
            cursor_row: 0,
            combining_char_col: 0,
            combining_char_row: 0,
            default_tabs: true,
            tabs: vec![false; width as usize],
            scrolling_region_top_row: 0,
            scrolling_region_bottom_row: height - 1,
            renditions: Renditions::new(0),
            hyperlink: Hyperlink::default(),
            save: SavedCursor::default(),
            next_print_will_wrap: false,
            origin_mode: false,
            auto_wrap_mode: true,
            insert_mode: false,
            cursor_visible: true,
            reverse_video: false,
            bracketed_paste: false,
            mouse_reporting_mode: MOUSE_REPORTING_NONE,
            mouse_focus_event: false,
            mouse_alternate_scroll: false,
            mouse_encoding_mode: MOUSE_ENCODING_DEFAULT,
            application_mode_cursor_keys: false,
        };
        ds.reinitialize_tabs(0);
        ds
    }

    fn reinitialize_tabs(&mut self, start: usize) {
        for i in start..self.tabs.len() {
            self.tabs[i] = i % 8 == 0;
        }
    }

    fn new_grapheme(&mut self) {
        self.combining_char_col = self.cursor_col;
        self.combining_char_row = self.cursor_row;
    }

    fn snap_cursor_to_border(&mut self) {
        if self.cursor_row < self.limit_top() {
            self.cursor_row = self.limit_top();
        }
        if self.cursor_row > self.limit_bottom() {
            self.cursor_row = self.limit_bottom();
        }
        if self.cursor_col < 0 {
            self.cursor_col = 0;
        }
        if self.cursor_col >= self.width {
            self.cursor_col = self.width - 1;
        }
    }

    pub fn move_row(&mut self, n: i32, relative: bool) {
        if relative {
            self.cursor_row += n;
        } else {
            self.cursor_row = n + self.limit_top();
        }
        self.snap_cursor_to_border();
        self.new_grapheme();
        self.next_print_will_wrap = false;
    }

    pub fn move_col(&mut self, n: i32, relative: bool, implicit: bool) {
        if implicit {
            self.new_grapheme();
        }
        if relative {
            self.cursor_col += n;
        } else {
            self.cursor_col = n;
        }
        if implicit {
            self.next_print_will_wrap = self.cursor_col >= self.width;
        }
        self.snap_cursor_to_border();
        if !implicit {
            self.new_grapheme();
            self.next_print_will_wrap = false;
        }
    }

    pub fn cursor_col(&self) -> i32 {
        self.cursor_col
    }
    pub fn cursor_row(&self) -> i32 {
        self.cursor_row
    }
    pub fn combining_char_col(&self) -> i32 {
        self.combining_char_col
    }
    pub fn combining_char_row(&self) -> i32 {
        self.combining_char_row
    }
    pub fn width(&self) -> i32 {
        self.width
    }
    pub fn height(&self) -> i32 {
        self.height
    }

    pub fn set_tab(&mut self) {
        let c = self.cursor_col as usize;
        self.tabs[c] = true;
    }

    pub fn clear_tab(&mut self, col: i32) {
        self.tabs[col as usize] = false;
    }

    pub fn clear_default_tabs(&mut self) {
        self.default_tabs = false;
    }

    pub fn get_next_tab(&self, mut count: i32) -> i32 {
        if count >= 0 {
            for i in (self.cursor_col + 1)..self.width {
                if self.tabs[i as usize] {
                    count -= 1;
                    if count == 0 {
                        return i;
                    }
                }
            }
            return -1;
        }
        let mut i = self.cursor_col - 1;
        while i > 0 {
            if self.tabs[i as usize] {
                count += 1;
                if count == 0 {
                    return i;
                }
            }
            i -= 1;
        }
        0
    }

    pub fn set_scrolling_region(&mut self, top: i32, bottom: i32) {
        if self.height < 1 {
            return;
        }
        self.scrolling_region_top_row = top.max(0);
        self.scrolling_region_bottom_row = bottom.min(self.height - 1);
        if self.scrolling_region_bottom_row < self.scrolling_region_top_row {
            self.scrolling_region_bottom_row = self.scrolling_region_top_row;
        }
        if self.origin_mode {
            self.snap_cursor_to_border();
            self.new_grapheme();
        }
    }

    pub fn scrolling_region_top_row(&self) -> i32 {
        self.scrolling_region_top_row
    }
    pub fn scrolling_region_bottom_row(&self) -> i32 {
        self.scrolling_region_bottom_row
    }

    pub fn limit_top(&self) -> i32 {
        if self.origin_mode { self.scrolling_region_top_row } else { 0 }
    }

    pub fn limit_bottom(&self) -> i32 {
        if self.origin_mode { self.scrolling_region_bottom_row } else { self.height - 1 }
    }

    pub fn hyperlink(&self) -> &Hyperlink {
        &self.hyperlink
    }
    pub fn set_hyperlink(&mut self, h: Hyperlink) {
        self.hyperlink = h;
    }
    pub fn set_foreground_color(&mut self, x: i32) {
        self.renditions.set_foreground_color(x);
    }
    pub fn set_background_color(&mut self, x: i32) {
        self.renditions.set_background_color(x);
    }
    pub fn add_rendition(&mut self, x: i32) {
        self.renditions.set_rendition(x);
    }
    pub fn renditions(&self) -> &Renditions {
        &self.renditions
    }
    pub fn background_rendition(&self) -> Color {
        self.renditions.background_rendition()
    }

    pub fn save_cursor(&mut self) {
        self.save = SavedCursor {
            cursor_col: self.cursor_col,
            cursor_row: self.cursor_row,
            renditions: self.renditions,
            auto_wrap_mode: self.auto_wrap_mode,
            origin_mode: self.origin_mode,
        };
    }

    pub fn restore_cursor(&mut self) {
        self.cursor_col = self.save.cursor_col;
        self.cursor_row = self.save.cursor_row;
        self.renditions = self.save.renditions;
        self.auto_wrap_mode = self.save.auto_wrap_mode;
        self.origin_mode = self.save.origin_mode;
        self.snap_cursor_to_border(); /* we could have resized in between */
        self.new_grapheme();
    }

    pub fn clear_saved_cursor(&mut self) {
        self.save = SavedCursor::default();
    }

    pub fn resize(&mut self, width: i32, height: i32) {
        if self.width != width || self.height != height {
            /* reset entire scrolling region on any resize */
            self.scrolling_region_top_row = 0;
            self.scrolling_region_bottom_row = height - 1;
        }
        let old_width = self.width as usize;
        self.tabs.resize(width as usize, false);
        if self.default_tabs {
            self.reinitialize_tabs(old_width.min(width as usize));
        }
        self.width = width;
        self.height = height;
        self.snap_cursor_to_border();
        /* invalidate combining char cell if necessary */
        if self.combining_char_col >= width || self.combining_char_row >= height {
            self.combining_char_col = -1;
            self.combining_char_row = -1;
        }
    }

    /// Used by the prediction overlay to place the cursor.
    pub fn set_cursor(&mut self, row: i32, col: i32) {
        self.move_row(row, false);
        self.move_col(col, false, false);
    }
}

#[derive(Clone, Debug)]
pub struct Framebuffer {
    rows: Vec<Rc<Row>>,
    icon_name: Vec<char>,
    window_title: Vec<char>,
    clipboard: Vec<char>,
    bell_count: u32,
    title_initialized: bool,
    pub ds: DrawState,
}

impl PartialEq for Framebuffer {
    fn eq(&self, x: &Self) -> bool {
        self.rows.len() == x.rows.len()
            && self.rows.iter().zip(x.rows.iter()).all(|(a, b)| Rc::ptr_eq(a, b) || **a == **b)
            && self.window_title == x.window_title
            && self.clipboard == x.clipboard
            && self.bell_count == x.bell_count
            && self.ds == x.ds
    }
}

impl Framebuffer {
    pub fn new(width: i32, height: i32) -> Self {
        assert!(width > 0 && height > 0);
        let row = Rc::new(Row::new(width as usize, 0));
        Framebuffer {
            rows: vec![row; height as usize],
            icon_name: Vec::new(),
            window_title: Vec::new(),
            clipboard: Vec::new(),
            bell_count: 0,
            title_initialized: false,
            ds: DrawState::new(width, height),
        }
    }

    fn newrow(&self) -> Rc<Row> {
        Rc::new(Row::new(self.ds.width() as usize, self.ds.background_rendition()))
    }

    pub fn rows(&self) -> &Vec<Rc<Row>> {
        &self.rows
    }

    pub fn row_rc(&self, row: i32) -> &Rc<Row> {
        let row = if row == -1 { self.ds.cursor_row() } else { row };
        &self.rows[row as usize]
    }

    pub fn get_row(&self, row: i32) -> &Row {
        self.row_rc(row)
    }

    pub fn get_cell(&self, row: i32, col: i32) -> &Cell {
        let row = if row == -1 { self.ds.cursor_row() } else { row };
        let col = if col == -1 { self.ds.cursor_col() } else { col };
        &self.rows[row as usize].cells[col as usize]
    }

    pub fn get_mutable_row(&mut self, row: i32) -> &mut Row {
        let row = if row == -1 { self.ds.cursor_row() } else { row };
        Rc::make_mut(&mut self.rows[row as usize])
    }

    pub fn get_mutable_cell(&mut self, row: i32, col: i32) -> &mut Cell {
        let col = if col == -1 { self.ds.cursor_col() } else { col };
        &mut self.get_mutable_row(row).cells[col as usize]
    }

    pub fn scroll(&mut self, n: i32) {
        if n >= 0 {
            self.delete_line(self.ds.scrolling_region_top_row(), n);
        } else {
            self.insert_line(self.ds.scrolling_region_top_row(), -n);
        }
    }

    pub fn move_rows_autoscroll(&mut self, rows: i32) {
        /* don't scroll if outside the scrolling region */
        if self.ds.cursor_row() < self.ds.scrolling_region_top_row()
            || self.ds.cursor_row() > self.ds.scrolling_region_bottom_row()
        {
            self.ds.move_row(rows, true);
            return;
        }
        if self.ds.cursor_row() + rows > self.ds.scrolling_region_bottom_row() {
            let n = self.ds.cursor_row() + rows - self.ds.scrolling_region_bottom_row();
            self.scroll(n);
            self.ds.move_row(-n, true);
        } else if self.ds.cursor_row() + rows < self.ds.scrolling_region_top_row() {
            let n = self.ds.cursor_row() + rows - self.ds.scrolling_region_top_row();
            self.scroll(n);
            self.ds.move_row(-n, true);
        }
        self.ds.move_row(rows, true);
    }

    pub fn has_combining_cell(&self) -> bool {
        let (c, r) = (self.ds.combining_char_col(), self.ds.combining_char_row());
        !(c < 0 || r < 0 || c >= self.ds.width() || r >= self.ds.height())
    }

    pub fn get_combining_cell(&mut self) -> Option<&mut Cell> {
        if !self.has_combining_cell() {
            return None; /* can happen if a resize came in between */
        }
        let (r, c) = (self.ds.combining_char_row(), self.ds.combining_char_col());
        Some(self.get_mutable_cell(r, c))
    }

    pub fn insert_line(&mut self, before_row: i32, count: i32) {
        let top = self.ds.scrolling_region_top_row();
        let bottom = self.ds.scrolling_region_bottom_row();
        if before_row < top || before_row > bottom + 1 {
            return;
        }
        let scroll = (bottom + 1 - before_row).min(count);
        if scroll <= 0 {
            return;
        }
        let start = (bottom + 1 - scroll) as usize;
        self.rows.drain(start..start + scroll as usize);
        let nr = self.newrow();
        for _ in 0..scroll {
            self.rows.insert(before_row as usize, nr.clone());
        }
    }

    pub fn delete_line(&mut self, row: i32, count: i32) {
        let top = self.ds.scrolling_region_top_row();
        let bottom = self.ds.scrolling_region_bottom_row();
        if row < top || row > bottom {
            return;
        }
        let scroll = (bottom + 1 - row).min(count);
        if scroll <= 0 {
            return;
        }
        self.rows.drain(row as usize..(row + scroll) as usize);
        let start = (bottom + 1 - scroll) as usize;
        let nr = self.newrow();
        for _ in 0..scroll {
            self.rows.insert(start, nr.clone());
        }
    }

    pub fn insert_cell(&mut self, row: i32, col: i32) {
        let bg = self.ds.background_rendition();
        self.get_mutable_row(row).insert_cell(col as usize, bg);
    }

    pub fn delete_cell(&mut self, row: i32, col: i32) {
        let bg = self.ds.background_rendition();
        self.get_mutable_row(row).delete_cell(col as usize, bg);
    }

    pub fn reset(&mut self) {
        let (w, h) = (self.ds.width(), self.ds.height());
        self.ds = DrawState::new(w, h);
        let nr = self.newrow();
        self.rows = vec![nr; h as usize];
        self.window_title.clear();
        self.clipboard.clear();
        /* do not reset bell_count */
    }

    pub fn soft_reset(&mut self) {
        self.ds.insert_mode = false;
        self.ds.origin_mode = false;
        self.ds.cursor_visible = true;
        self.ds.application_mode_cursor_keys = false;
        let h = self.ds.height();
        self.ds.set_scrolling_region(0, h - 1);
        self.ds.add_rendition(0);
        self.ds.set_hyperlink(Hyperlink::default());
        self.ds.clear_saved_cursor();
    }

    pub fn resize(&mut self, width: i32, height: i32) {
        assert!(width > 0 && height > 0);
        let old_height = self.ds.height();
        let old_width = self.ds.width();
        self.ds.resize(width, height);

        let blankrow = self.newrow();
        if old_height != height {
            self.rows.resize(height as usize, blankrow.clone());
        }
        if old_width == width {
            return;
        }
        let bg = self.ds.background_rendition();
        for r in self.rows.iter_mut() {
            if Rc::ptr_eq(r, &blankrow) {
                break;
            }
            let mut nr = (**r).clone();
            nr.set_wrap(false);
            nr.cells.resize(width as usize, Cell::new(bg));
            *r = Rc::new(nr);
        }
        // rows that were the shared blank row still have the new width already
    }

    pub fn reset_cell_at(&mut self, row: i32, col: i32) {
        let bg = self.ds.background_rendition();
        self.get_mutable_cell(row, col).reset(bg);
    }

    pub fn reset_row(&mut self, row: i32) {
        let bg = self.ds.background_rendition();
        self.get_mutable_row(row).reset(bg);
    }

    pub fn ring_bell(&mut self) {
        self.bell_count += 1;
    }
    pub fn bell_count(&self) -> u32 {
        self.bell_count
    }
    pub fn set_title_initialized(&mut self) {
        self.title_initialized = true;
    }
    pub fn is_title_initialized(&self) -> bool {
        self.title_initialized
    }
    pub fn set_icon_name(&mut self, s: Vec<char>) {
        self.icon_name = s;
    }
    pub fn set_window_title(&mut self, s: Vec<char>) {
        self.window_title = s;
    }
    pub fn set_clipboard(&mut self, s: Vec<char>) {
        self.clipboard = s;
    }
    pub fn icon_name(&self) -> &[char] {
        &self.icon_name
    }
    pub fn window_title(&self) -> &[char] {
        &self.window_title
    }
    pub fn clipboard(&self) -> &[char] {
        &self.clipboard
    }

    pub fn prefix_window_title(&mut self, s: &[char]) {
        if self.icon_name == self.window_title {
            /* preserve equivalence */
            self.icon_name.splice(0..0, s.iter().copied());
        }
        self.window_title.splice(0..0, s.iter().copied());
    }

    /// Plain-text rendering of the screen (for headless testing).
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for r in &self.rows {
            let mut line = String::new();
            let mut skip = 0;
            for c in &r.cells {
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                c.print_grapheme(&mut line);
                if c.wide() {
                    skip = 1;
                }
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }
}

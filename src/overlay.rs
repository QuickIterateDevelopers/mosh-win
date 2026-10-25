// SPDX-License-Identifier: GPL-3.0-or-later
// mosh-win: Copyright 2026 the mosh-win authors.
// Ported from mosh (https://mosh.org): Copyright 2012 Keith Winstein and the mosh authors.

//! Local overlays drawn on top of the server's screen: predictive echo,
//! the notification bar and the title prefix (port of mosh's terminaloverlay.cc).

use crate::terminal::emulator::wcwidth;
use crate::terminal::framebuffer::{Cell, Framebuffer, BOLD, UNDERLINED};
use crate::timestamp::timestamp;
use crate::transport::ACK_INTERVAL;

#[derive(PartialEq, Eq, Clone, Copy)]
enum Validity {
    Pending,
    Correct,
    CorrectNoCredit,
    IncorrectOrExpired,
    Inactive,
}

#[derive(Clone)]
struct ConditionalOverlay {
    expiration_frame: u64,
    col: i32,
    active: bool,
    tentative_until_epoch: u64,
    prediction_time: u64,
}

impl ConditionalOverlay {
    fn new(exp: u64, col: i32, tentative: u64) -> Self {
        ConditionalOverlay { expiration_frame: exp, col, active: false, tentative_until_epoch: tentative, prediction_time: u64::MAX }
    }
    fn tentative(&self, confirmed_epoch: u64) -> bool {
        self.tentative_until_epoch > confirmed_epoch
    }
    fn reset(&mut self) {
        self.expiration_frame = u64::MAX;
        self.tentative_until_epoch = u64::MAX;
        self.active = false;
    }
    fn expire(&mut self, exp: u64, now: u64) {
        self.expiration_frame = exp;
        self.prediction_time = now;
    }
}

#[derive(Clone)]
struct ConditionalCursorMove {
    base: ConditionalOverlay,
    row: i32,
}

impl ConditionalCursorMove {
    fn new(exp: u64, row: i32, col: i32, tentative: u64) -> Self {
        ConditionalCursorMove { base: ConditionalOverlay::new(exp, col, tentative), row }
    }

    fn apply(&self, fb: &mut Framebuffer, confirmed_epoch: u64) {
        if !self.base.active || self.base.tentative(confirmed_epoch) {
            return;
        }
        if self.row >= fb.ds.height() || self.base.col >= fb.ds.width() || fb.ds.origin_mode {
            return;
        }
        fb.ds.set_cursor(self.row, self.base.col);
    }

    fn get_validity(&self, fb: &Framebuffer, late_ack: u64) -> Validity {
        if !self.base.active {
            return Validity::Inactive;
        }
        if self.row >= fb.ds.height() || self.base.col >= fb.ds.width() {
            return Validity::IncorrectOrExpired;
        }
        if late_ack >= self.base.expiration_frame {
            if fb.ds.cursor_col() == self.base.col && fb.ds.cursor_row() == self.row {
                return Validity::Correct;
            }
            return Validity::IncorrectOrExpired;
        }
        Validity::Pending
    }
}

#[derive(Clone)]
struct ConditionalOverlayCell {
    base: ConditionalOverlay,
    replacement: Cell,
    unknown: bool,
    original_contents: Vec<Cell>,
}

impl ConditionalOverlayCell {
    fn new(exp: u64, col: i32, tentative: u64) -> Self {
        ConditionalOverlayCell {
            base: ConditionalOverlay::new(exp, col, tentative),
            replacement: Cell::new(0),
            unknown: false,
            original_contents: Vec::new(),
        }
    }

    fn reset(&mut self) {
        self.unknown = false;
        self.original_contents.clear();
        self.base.reset();
    }

    fn reset_with_orig(&mut self) {
        if !self.base.active || self.unknown {
            self.reset();
            return;
        }
        self.original_contents.push(self.replacement.clone());
        self.base.reset();
    }

    fn apply(&self, fb: &mut Framebuffer, confirmed_epoch: u64, row: i32, mut flag: bool) {
        let col = self.base.col;
        if !self.base.active || row >= fb.ds.height() || col >= fb.ds.width() {
            return;
        }
        if self.base.tentative(confirmed_epoch) {
            return;
        }
        if self.replacement.is_blank() && fb.get_cell(row, col).is_blank() {
            flag = false;
        }
        if self.unknown {
            if flag && col != fb.ds.width() - 1 {
                fb.get_mutable_cell(row, col).renditions_mut().set_attribute(UNDERLINED, true);
            }
            return;
        }
        if *fb.get_cell(row, col) != self.replacement {
            *fb.get_mutable_cell(row, col) = self.replacement.clone();
            if flag {
                fb.get_mutable_cell(row, col).renditions_mut().set_attribute(UNDERLINED, true);
            }
        }
    }

    fn get_validity(&self, fb: &Framebuffer, row: i32, late_ack: u64) -> Validity {
        if !self.base.active {
            return Validity::Inactive;
        }
        if row >= fb.ds.height() || self.base.col >= fb.ds.width() {
            return Validity::IncorrectOrExpired;
        }
        let current = fb.get_cell(row, self.base.col);
        /* see if it hasn't been updated yet */
        if late_ack < self.base.expiration_frame {
            return Validity::Pending;
        }
        if self.unknown {
            return Validity::CorrectNoCredit;
        }
        if self.replacement.is_blank() {
            /* too easy for this to trigger falsely */
            return Validity::CorrectNoCredit;
        }
        if current.contents_match(&self.replacement) {
            if self.original_contents.iter().any(|c| c.contents_match(&self.replacement)) {
                return Validity::CorrectNoCredit;
            }
            return Validity::Correct;
        }
        Validity::IncorrectOrExpired
    }
}

#[derive(Clone)]
struct ConditionalOverlayRow {
    row_num: i32,
    overlay_cells: Vec<ConditionalOverlayCell>,
}

impl ConditionalOverlayRow {
    fn apply(&self, fb: &mut Framebuffer, confirmed_epoch: u64, flag: bool) {
        for c in &self.overlay_cells {
            c.apply(fb, confirmed_epoch, self.row_num, flag);
        }
    }
}

/* ---------------- notification engine ---------------- */

pub struct NotificationEngine {
    last_word_from_server: u64,
    last_acked_state: u64,
    escape_key_string: String,
    message: String,
    message_is_network_error: bool,
    message_expiration: u64,
    show_quit_keystroke: bool,
}

fn human_readable_duration(num_seconds: i64, seconds_abbr: &str) -> String {
    if num_seconds < 60 {
        format!("{num_seconds} {seconds_abbr}")
    } else if num_seconds < 3600 {
        format!("{}:{:02}", num_seconds / 60, num_seconds % 60)
    } else {
        format!("{}:{:02}:{:02}", num_seconds / 3600, (num_seconds / 60) % 60, num_seconds % 60)
    }
}

impl NotificationEngine {
    fn new() -> Self {
        NotificationEngine {
            last_word_from_server: timestamp(),
            last_acked_state: timestamp(),
            escape_key_string: String::new(),
            message: String::new(),
            message_is_network_error: false,
            message_expiration: u64::MAX,
            show_quit_keystroke: true,
        }
    }

    fn server_late(&self, ts: u64) -> bool {
        ts.wrapping_sub(self.last_word_from_server) > 6500
    }
    fn reply_late(&self, ts: u64) -> bool {
        ts.wrapping_sub(self.last_acked_state) > 10000
    }
    fn need_countup(&self, ts: u64) -> bool {
        self.server_late(ts) || self.reply_late(ts)
    }

    pub fn notification_string(&self) -> &str {
        &self.message
    }
    pub fn server_heard(&mut self, t: u64) {
        self.last_word_from_server = t;
    }
    pub fn server_acked(&mut self, t: u64) {
        self.last_acked_state = t;
    }

    pub fn set_notification_string(&mut self, msg: &str, permanent: bool, show_quit_keystroke: bool) {
        self.message = msg.to_string();
        self.message_expiration = if permanent { u64::MAX } else { timestamp() + 1000 };
        self.message_is_network_error = false;
        self.show_quit_keystroke = show_quit_keystroke;
    }

    pub fn set_escape_key_string(&mut self, name: &str) {
        self.escape_key_string = format!(" [To quit: {name} .]");
    }

    pub fn set_network_error(&mut self, s: &str) {
        self.message = s.to_string();
        self.message_is_network_error = true;
        self.message_expiration = timestamp() + ACK_INTERVAL + 100;
    }

    pub fn clear_network_error(&mut self) {
        if self.message_is_network_error {
            self.message_expiration = self.message_expiration.min(timestamp() + 1000);
        }
    }

    fn adjust_message(&mut self) {
        if timestamp() >= self.message_expiration {
            self.message.clear();
        }
    }

    fn wait_time(&self) -> u64 {
        let now = timestamp();
        let mut next_expiry = i32::MAX as u64;
        next_expiry = next_expiry.min(self.message_expiration.wrapping_sub(now));
        if self.need_countup(now) {
            let mut countup_interval = 1000;
            if now.wrapping_sub(self.last_word_from_server) > 60000 {
                /* If we've been disconnected for 60 seconds, save power by updating less often. */
                countup_interval = ACK_INTERVAL;
            }
            next_expiry = next_expiry.min(countup_interval);
        }
        next_expiry
    }

    fn apply(&self, fb: &mut Framebuffer) {
        let now = timestamp();
        let time_expired = self.need_countup(now);
        if self.message.is_empty() && !time_expired {
            return;
        }

        /* hide cursor if necessary */
        if fb.ds.cursor_row() == 0 {
            fb.ds.cursor_visible = false;
        }

        /* draw bar across top of screen */
        let mut notification_bar = Cell::new(0);
        notification_bar.renditions_mut().set_foreground_color(7);
        notification_bar.renditions_mut().set_background_color(4);
        notification_bar.append(' ');
        for i in 0..fb.ds.width() {
            *fb.get_mutable_cell(0, i) = notification_bar.clone();
        }

        let since_heard = now.wrapping_sub(self.last_word_from_server) as f64 / 1000.0;
        let since_ack = now.wrapping_sub(self.last_acked_state) as f64 / 1000.0;
        let mut time_elapsed = since_heard;
        let mut explanation = "contact";
        if self.reply_late(now) && !self.server_late(now) {
            time_elapsed = since_ack;
            explanation = "reply";
        }
        let keystroke_str = if self.show_quit_keystroke { self.escape_key_string.as_str() } else { "" };

        let string_to_draw = if self.message.is_empty() && time_expired {
            format!(
                "mosh: Last {} {} ago.{}",
                explanation,
                human_readable_duration(time_elapsed as i64, "seconds"),
                keystroke_str
            )
        } else if !self.message.is_empty() && !time_expired {
            format!("mosh: {}{}", self.message, keystroke_str)
        } else {
            format!(
                "mosh: {} ({} without {}.){}",
                self.message,
                human_readable_duration(time_elapsed as i64, "s"),
                explanation,
                keystroke_str
            )
        };

        let mut overlay_col = 0;
        let mut combining_col: Option<i32> = Some(0);
        for ch in string_to_draw.chars() {
            if overlay_col >= fb.ds.width() {
                break;
            }
            let chwidth = wcwidth(ch);
            match chwidth {
                1 | 2 => {
                    let cell = fb.get_mutable_cell(0, overlay_col);
                    cell.reset(0);
                    cell.renditions_mut().set_attribute(BOLD, true);
                    cell.renditions_mut().set_foreground_color(7);
                    cell.renditions_mut().set_background_color(4);
                    cell.append(ch);
                    cell.set_wide(chwidth == 2);
                    combining_col = Some(overlay_col);
                    overlay_col += chwidth;
                }
                0 => {
                    let Some(cc) = combining_col else { continue };
                    let cell = fb.get_mutable_cell(0, cc);
                    if cell.empty() {
                        cell.set_fallback(true);
                        overlay_col += 1;
                    }
                    if !cell.full() {
                        cell.append(ch);
                    }
                }
                _ => {}
            }
        }
    }
}

/* ---------------- prediction engine ---------------- */

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DisplayPreference {
    Always,
    Never,
    Adaptive,
    Experimental,
}

const SRTT_TRIGGER_LOW: u64 = 20;
const SRTT_TRIGGER_HIGH: u64 = 30;
const FLAG_TRIGGER_LOW: u64 = 50;
const FLAG_TRIGGER_HIGH: u64 = 80;
const GLITCH_THRESHOLD: u64 = 250;
const GLITCH_REPAIR_COUNT: u32 = 10;
const GLITCH_REPAIR_MININTERVAL: u64 = 150;
const GLITCH_FLAG_THRESHOLD: u64 = 5000;

enum ParsedAction {
    Print(char),
    Execute(u8),
    Esc,
    Csi(char),
}

#[derive(Default)]
struct ActionCollector(Vec<ParsedAction>);

impl vte::Perform for ActionCollector {
    fn print(&mut self, c: char) {
        self.0.push(ParsedAction::Print(c));
    }
    fn execute(&mut self, byte: u8) {
        self.0.push(ParsedAction::Execute(byte));
    }
    fn csi_dispatch(&mut self, _: &vte::Params, _: &[u8], _: bool, action: char) {
        self.0.push(ParsedAction::Csi(action));
    }
    fn esc_dispatch(&mut self, _: &[u8], _: bool, _: u8) {
        self.0.push(ParsedAction::Esc);
    }
}

pub struct PredictionEngine {
    last_byte: u8,
    parser: vte::Parser,
    overlays: Vec<ConditionalOverlayRow>,
    cursors: Vec<ConditionalCursorMove>,
    local_frame_sent: u64,
    local_frame_acked: u64,
    local_frame_late_acked: u64,
    prediction_epoch: u64,
    confirmed_epoch: u64,
    flagging: bool,
    srtt_trigger: bool,
    glitch_trigger: u32,
    last_quick_confirmation: u64,
    send_interval: u64,
    last_height: i32,
    last_width: i32,
    display_preference: DisplayPreference,
    predict_overwrite: bool,
}

impl PredictionEngine {
    fn new() -> Self {
        PredictionEngine {
            last_byte: 0,
            parser: vte::Parser::new(),
            overlays: Vec::new(),
            cursors: Vec::new(),
            local_frame_sent: 0,
            local_frame_acked: 0,
            local_frame_late_acked: 0,
            prediction_epoch: 1,
            confirmed_epoch: 0,
            flagging: false,
            srtt_trigger: false,
            glitch_trigger: 0,
            last_quick_confirmation: 0,
            send_interval: 250,
            last_height: 0,
            last_width: 0,
            display_preference: DisplayPreference::Adaptive,
            predict_overwrite: false,
        }
    }

    pub fn set_display_preference(&mut self, p: DisplayPreference) {
        self.display_preference = p;
    }
    pub fn set_predict_overwrite(&mut self, o: bool) {
        self.predict_overwrite = o;
    }
    pub fn set_local_frame_sent(&mut self, x: u64) {
        self.local_frame_sent = x;
    }
    pub fn set_local_frame_acked(&mut self, x: u64) {
        self.local_frame_acked = x;
    }
    pub fn set_local_frame_late_acked(&mut self, x: u64) {
        self.local_frame_late_acked = x;
    }
    pub fn set_send_interval(&mut self, x: u64) {
        self.send_interval = x;
    }

    fn cursor(&mut self) -> &mut ConditionalCursorMove {
        self.cursors.last_mut().expect("cursor prediction")
    }

    fn active(&self) -> bool {
        !self.cursors.is_empty() || self.overlays.iter().any(|r| r.overlay_cells.iter().any(|c| c.base.active))
    }

    fn timing_tests_necessary(&self) -> bool {
        /* Are there any timing-based triggers that haven't fired yet? */
        !(self.glitch_trigger != 0 && self.flagging)
    }

    fn wait_time(&self) -> u64 {
        if self.timing_tests_necessary() && self.active() { 50 } else { i32::MAX as u64 }
    }

    fn apply(&self, fb: &mut Framebuffer) {
        if self.display_preference == DisplayPreference::Never
            || !(self.srtt_trigger
                || self.glitch_trigger != 0
                || self.display_preference == DisplayPreference::Always
                || self.display_preference == DisplayPreference::Experimental)
        {
            return;
        }
        for c in &self.cursors {
            c.apply(fb, self.confirmed_epoch);
        }
        for r in &self.overlays {
            r.apply(fb, self.confirmed_epoch, self.flagging);
        }
    }

    fn kill_epoch(&mut self, epoch: u64, fb: &Framebuffer) {
        self.cursors.retain(|c| !c.base.tentative(epoch - 1));
        let mut c = ConditionalCursorMove::new(
            self.local_frame_sent + 1,
            fb.ds.cursor_row(),
            fb.ds.cursor_col(),
            self.prediction_epoch,
        );
        c.base.active = true;
        self.cursors.push(c);
        for r in self.overlays.iter_mut() {
            for j in r.overlay_cells.iter_mut() {
                if j.base.tentative(epoch - 1) {
                    j.reset();
                }
            }
        }
        self.become_tentative();
    }

    pub fn reset(&mut self) {
        self.cursors.clear();
        self.overlays.clear();
        self.become_tentative();
    }

    fn init_cursor(&mut self, fb: &Framebuffer) {
        if self.cursors.is_empty() {
            /* initialize new cursor prediction */
            let mut c = ConditionalCursorMove::new(
                self.local_frame_sent + 1,
                fb.ds.cursor_row(),
                fb.ds.cursor_col(),
                self.prediction_epoch,
            );
            c.base.active = true;
            self.cursors.push(c);
        } else if self.cursors.last().unwrap().base.tentative_until_epoch != self.prediction_epoch {
            let (row, col) = {
                let c = self.cursors.last().unwrap();
                (c.row, c.base.col)
            };
            let mut c = ConditionalCursorMove::new(self.local_frame_sent + 1, row, col, self.prediction_epoch);
            c.base.active = true;
            self.cursors.push(c);
        }
    }

    fn cull(&mut self, fb: &Framebuffer) {
        if self.display_preference == DisplayPreference::Never {
            return;
        }
        if self.last_height != fb.ds.height() || self.last_width != fb.ds.width() {
            self.last_height = fb.ds.height();
            self.last_width = fb.ds.width();
            self.reset();
        }

        let now = timestamp();

        /* control srtt_trigger with hysteresis */
        if self.send_interval > SRTT_TRIGGER_HIGH {
            self.srtt_trigger = true;
        } else if self.srtt_trigger && self.send_interval <= SRTT_TRIGGER_LOW && !self.active() {
            self.srtt_trigger = false;
        }

        /* control underlining with hysteresis */
        if self.send_interval > FLAG_TRIGGER_HIGH {
            self.flagging = true;
        } else if self.send_interval <= FLAG_TRIGGER_LOW {
            self.flagging = false;
        }

        /* really big glitches also activate underlining */
        if self.glitch_trigger > GLITCH_REPAIR_COUNT {
            self.flagging = true;
        }

        let experimental = self.display_preference == DisplayPreference::Experimental;

        /* go through cell predictions */
        let mut i = 0;
        while i < self.overlays.len() {
            let row_num = self.overlays[i].row_num;
            if row_num < 0 || row_num >= fb.ds.height() {
                self.overlays.remove(i);
                continue;
            }
            let mut j = 0;
            while j < self.overlays[i].overlay_cells.len() {
                let validity = self.overlays[i].overlay_cells[j].get_validity(fb, row_num, self.local_frame_late_acked);
                match validity {
                    Validity::IncorrectOrExpired => {
                        let cell = &mut self.overlays[i].overlay_cells[j];
                        if cell.base.tentative(self.confirmed_epoch) {
                            if experimental {
                                cell.reset();
                            } else {
                                let epoch = cell.base.tentative_until_epoch;
                                self.kill_epoch(epoch, fb);
                            }
                        } else if experimental {
                            cell.reset();
                        } else {
                            self.reset();
                            return;
                        }
                    }
                    Validity::Correct => {
                        let cell = &self.overlays[i].overlay_cells[j];
                        if cell.base.tentative_until_epoch > self.confirmed_epoch {
                            self.confirmed_epoch = cell.base.tentative_until_epoch;
                        }
                        /* When predictions come in quickly, slowly take away the glitch trigger. */
                        if now.wrapping_sub(cell.base.prediction_time) < GLITCH_THRESHOLD
                            && self.glitch_trigger > 0
                            && now - GLITCH_REPAIR_MININTERVAL >= self.last_quick_confirmation
                        {
                            self.glitch_trigger -= 1;
                            self.last_quick_confirmation = now;
                        }
                        /* match rest of row to the actual renditions */
                        let actual = *fb.get_cell(row_num, cell.base.col).renditions();
                        for k in self.overlays[i].overlay_cells[j..].iter_mut() {
                            *k.replacement.renditions_mut() = actual;
                        }
                        self.overlays[i].overlay_cells[j].reset();
                    }
                    Validity::CorrectNoCredit => {
                        self.overlays[i].overlay_cells[j].reset();
                    }
                    Validity::Pending => {
                        /* When a prediction takes a long time to be confirmed, we
                        activate the predictions even if SRTT is low */
                        let age = now.wrapping_sub(self.overlays[i].overlay_cells[j].base.prediction_time);
                        if age >= GLITCH_FLAG_THRESHOLD {
                            self.glitch_trigger = GLITCH_REPAIR_COUNT * 2; /* display and underline */
                        } else if age >= GLITCH_THRESHOLD && self.glitch_trigger < GLITCH_REPAIR_COUNT {
                            self.glitch_trigger = GLITCH_REPAIR_COUNT; /* just display */
                        }
                    }
                    Validity::Inactive => {}
                }
                j += 1;
            }
            i += 1;
        }

        /* go through cursor predictions */
        if let Some(c) = self.cursors.last() {
            if c.get_validity(fb, self.local_frame_late_acked) == Validity::IncorrectOrExpired {
                if experimental {
                    self.cursors.clear();
                } else {
                    self.reset();
                    return;
                }
            }
        }

        let late = self.local_frame_late_acked;
        self.cursors.retain(|c| c.get_validity(fb, late) == Validity::Pending);
    }

    fn get_or_make_row(&mut self, row_num: i32, num_cols: i32) -> usize {
        if let Some(i) = self.overlays.iter().position(|r| r.row_num == row_num) {
            return i;
        }
        let cells = (0..num_cols).map(|i| ConditionalOverlayCell::new(0, i, self.prediction_epoch)).collect();
        self.overlays.push(ConditionalOverlayRow { row_num, overlay_cells: cells });
        self.overlays.len() - 1
    }

    pub fn new_user_byte(&mut self, mut the_byte: u8, fb: &Framebuffer) {
        if self.display_preference == DisplayPreference::Never {
            return;
        }
        if self.display_preference == DisplayPreference::Experimental {
            self.prediction_epoch = self.confirmed_epoch;
        }

        self.cull(fb);

        let now = timestamp();

        /* translate application-mode cursor control function to ANSI cursor control sequence */
        if self.last_byte == 0x1b && the_byte == b'O' {
            the_byte = b'[';
        }
        self.last_byte = the_byte;

        let mut collector = ActionCollector::default();
        if the_byte == 0x7f {
            // vte ignores DEL in the ground state; mosh's parser reports it as a print
            collector.0.push(ParsedAction::Print('\x7f'));
        } else {
            self.parser.advance(&mut collector, the_byte);
        }

        for act in collector.0 {
            match act {
                ParsedAction::Print(ch) => {
                    /* make new prediction */
                    self.init_cursor(fb);
                    if ch == '\x7f' {
                        self.predict_backspace(fb, now);
                    } else if (ch as u32) < 0x20 || wcwidth(ch) != 1 {
                        /* unknown print */
                        self.become_tentative();
                    } else {
                        self.predict_char(ch, fb, now);
                    }
                }
                ParsedAction::Execute(b) => {
                    if b == 0x0d {
                        self.become_tentative();
                        self.newline_carriage_return(fb);
                    } else {
                        self.become_tentative();
                    }
                }
                ParsedAction::Esc => self.become_tentative(),
                ParsedAction::Csi(c) => {
                    if c == 'C' {
                        /* right arrow */
                        self.init_cursor(fb);
                        let width = fb.ds.width();
                        let exp = self.local_frame_sent + 1;
                        let cur = self.cursor();
                        if cur.base.col < width - 1 {
                            cur.base.col += 1;
                            cur.base.expire(exp, now);
                        }
                    } else if c == 'D' {
                        /* left arrow */
                        self.init_cursor(fb);
                        let exp = self.local_frame_sent + 1;
                        let cur = self.cursor();
                        if cur.base.col > 0 {
                            cur.base.col -= 1;
                            cur.base.expire(exp, now);
                        }
                    } else {
                        self.become_tentative();
                    }
                }
            }
        }
    }

    fn predict_backspace(&mut self, fb: &Framebuffer, now: u64) {
        let width = fb.ds.width();
        let exp = self.local_frame_sent + 1;
        let epoch = self.prediction_epoch;
        let (crow, ccol) = {
            let c = self.cursors.last().unwrap();
            (c.row, c.base.col)
        };
        let ri = self.get_or_make_row(crow, width);
        if ccol <= 0 {
            return;
        }
        let ccol = ccol - 1;
        {
            let c = self.cursor();
            c.base.col = ccol;
            c.base.expire(exp, now);
        }

        if self.predict_overwrite {
            let orig_cell = fb.get_cell(-1, -1).clone();
            let cell = &mut self.overlays[ri].overlay_cells[ccol as usize];
            cell.reset_with_orig();
            cell.base.active = true;
            cell.base.tentative_until_epoch = epoch;
            cell.base.expire(exp, now);
            cell.original_contents.push(orig_cell.clone());
            cell.replacement = orig_cell;
            cell.replacement.clear();
            cell.replacement.append(' ');
        } else {
            for i in ccol..width {
                let next_info = if i + 2 < width {
                    let next = &self.overlays[ri].overlay_cells[(i + 1) as usize];
                    Some((next.base.active, next.unknown, next.replacement.clone()))
                } else {
                    None
                };
                let orig = fb.get_cell(crow, i).clone();
                let cell = &mut self.overlays[ri].overlay_cells[i as usize];
                cell.reset_with_orig();
                cell.base.active = true;
                cell.base.tentative_until_epoch = epoch;
                cell.base.expire(exp, now);
                cell.original_contents.push(orig);
                match next_info {
                    Some((true, true, _)) => cell.unknown = true,
                    Some((true, false, rep)) => {
                        cell.unknown = false;
                        cell.replacement = rep;
                    }
                    Some((false, _, _)) => {
                        cell.unknown = false;
                        cell.replacement = fb.get_cell(crow, i + 1).clone();
                    }
                    None => cell.unknown = true,
                }
            }
        }
    }

    fn predict_char(&mut self, ch: char, fb: &Framebuffer, now: u64) {
        let width = fb.ds.width();
        let exp = self.local_frame_sent + 1;
        let (crow, ccol) = {
            let c = self.cursors.last().unwrap();
            (c.row, c.base.col)
        };
        if crow < 0 || ccol < 0 || crow >= fb.ds.height() || ccol >= width {
            self.become_tentative();
            return;
        }
        let ri = self.get_or_make_row(crow, width);

        if ccol + 1 >= width {
            /* prediction in the last column is tricky */
            self.become_tentative();
        }
        let epoch = self.prediction_epoch;

        /* do the insert */
        let rightmost_column = if self.predict_overwrite { ccol } else { width - 1 };
        let mut i = rightmost_column;
        while i > ccol {
            let prev = &self.overlays[ri].overlay_cells[(i - 1) as usize];
            let prev_info = (prev.base.active, prev.unknown, prev.replacement.clone());
            let orig = fb.get_cell(crow, i).clone();
            let cell = &mut self.overlays[ri].overlay_cells[i as usize];
            cell.reset_with_orig();
            cell.base.active = true;
            cell.base.tentative_until_epoch = epoch;
            cell.base.expire(exp, now);
            cell.original_contents.push(orig);
            if i == width - 1 {
                cell.unknown = true;
            } else if prev_info.0 {
                if prev_info.1 {
                    cell.unknown = true;
                } else {
                    cell.unknown = false;
                    cell.replacement = prev_info.2;
                }
            } else {
                cell.unknown = false;
                cell.replacement = fb.get_cell(crow, i - 1).clone();
            }
            i -= 1;
        }

        let mut renditions = *fb.ds.renditions();
        /* heuristic: match renditions of character to the left */
        if ccol > 0 {
            let prev = &self.overlays[ri].overlay_cells[(ccol - 1) as usize];
            renditions = if prev.base.active && !prev.unknown {
                *prev.replacement.renditions()
            } else {
                *fb.get_cell(crow, ccol - 1).renditions()
            };
        }
        let orig = fb.get_cell(crow, ccol).clone();
        let cell = &mut self.overlays[ri].overlay_cells[ccol as usize];
        cell.reset_with_orig();
        cell.base.active = true;
        cell.base.tentative_until_epoch = epoch;
        cell.base.expire(exp, now);
        *cell.replacement.renditions_mut() = renditions;
        cell.replacement.clear();
        cell.replacement.append(ch);
        cell.original_contents.push(orig);

        self.cursor().base.expire(exp, now);

        /* do we need to wrap? */
        if ccol < width - 1 {
            self.cursor().base.col += 1;
        } else {
            self.become_tentative();
            self.newline_carriage_return(fb);
        }
    }

    fn newline_carriage_return(&mut self, fb: &Framebuffer) {
        let now = timestamp();
        self.init_cursor(fb);
        self.cursor().base.col = 0;
        let row = self.cursors.last().unwrap().row;
        if row == fb.ds.height() - 1 {
            /* make blank prediction for last row */
            let ri = self.get_or_make_row(row, fb.ds.width());
            let (exp, epoch) = (self.local_frame_sent + 1, self.prediction_epoch);
            for j in self.overlays[ri].overlay_cells.iter_mut() {
                j.base.active = true;
                j.base.tentative_until_epoch = epoch;
                j.base.expire(exp, now);
                j.replacement.clear();
            }
        } else {
            self.cursor().row += 1;
        }
    }

    fn become_tentative(&mut self) {
        if self.display_preference != DisplayPreference::Experimental {
            self.prediction_epoch += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::emulator::act;

    fn row_text(fb: &Framebuffer, row: i32) -> String {
        let mut s = String::new();
        for x in 0..fb.ds.width() {
            fb.get_cell(row, x).print_grapheme(&mut s);
        }
        s.trim_end().to_string()
    }

    #[test]
    fn predicts_typing_and_confirms() {
        let mut server = Framebuffer::new(40, 5);
        act(&mut server, b"$ ");
        let mut p = PredictionEngine::new();
        p.set_display_preference(DisplayPreference::Always);
        p.set_local_frame_sent(9);

        // predictions stay hidden (tentative) until one in the epoch is confirmed
        p.new_user_byte(b'l', &server);
        let mut shown = server.clone();
        p.apply(&mut shown);
        assert_eq!(row_text(&shown, 0), "$");
        act(&mut server, b"l");
        p.set_local_frame_late_acked(10);
        p.cull(&server);

        p.set_local_frame_sent(10);
        for &b in b"s -l" {
            p.new_user_byte(b, &server);
        }
        let mut shown = server.clone();
        p.apply(&mut shown);
        assert_eq!(row_text(&shown, 0), "$ ls -l");
        assert_eq!(shown.ds.cursor_col(), 7);

        // backspace is predicted too
        p.new_user_byte(0x7f, &server);
        let mut shown = server.clone();
        p.apply(&mut shown);
        assert_eq!(row_text(&shown, 0), "$ ls -");
        assert_eq!(shown.ds.cursor_col(), 6);

        // the server echoes and acknowledges: predictions retire
        act(&mut server, b"s -");
        p.set_local_frame_late_acked(11);
        p.cull(&server);
        assert!(!p.active());
    }

    #[test]
    fn wrong_prediction_is_withdrawn() {
        let mut server = Framebuffer::new(40, 5);
        act(&mut server, b"Password: ");
        let mut p = PredictionEngine::new();
        p.set_display_preference(DisplayPreference::Always);
        p.set_local_frame_sent(3);
        for &b in b"abc" {
            p.new_user_byte(b, &server);
        }
        // the server did not echo (password prompt)
        p.set_local_frame_late_acked(4);
        p.cull(&server);
        let mut shown = server.clone();
        p.apply(&mut shown);
        assert_eq!(row_text(&shown, 0), "Password:");
    }
}

/* ---------------- overlay manager ---------------- */

pub struct OverlayManager {
    pub notifications: NotificationEngine,
    pub predictions: PredictionEngine,
    title_prefix: Vec<char>,
}

impl OverlayManager {
    pub fn new() -> Self {
        OverlayManager { notifications: NotificationEngine::new(), predictions: PredictionEngine::new(), title_prefix: Vec::new() }
    }

    pub fn apply(&mut self, fb: &mut Framebuffer) {
        self.predictions.cull(fb);
        self.predictions.apply(fb);
        self.notifications.adjust_message();
        self.notifications.apply(fb);
        fb.prefix_window_title(&self.title_prefix);
    }

    pub fn set_title_prefix(&mut self, s: &str) {
        self.title_prefix = s.chars().collect();
    }

    pub fn wait_time(&self) -> u64 {
        self.notifications.wait_time().min(self.predictions.wait_time())
    }
}

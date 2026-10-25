// SPDX-License-Identifier: GPL-3.0-or-later
// mosh-win: Copyright 2026 the mosh-win authors.
// Ported from mosh (https://mosh.org): Copyright 2012 Keith Winstein and the mosh authors.

//! Windows console handling: raw VT input, VT output, and window size.

use std::io::Write;
use std::sync::mpsc::Sender;

use windows_sys::Win32::System::Console::*;

use crate::client::Event;

type Handle = windows_sys::Win32::Foundation::HANDLE;

fn std_handle(which: STD_HANDLE) -> Handle {
    unsafe { GetStdHandle(which) }
}

/// Saves the console state on creation and restores it on drop.
pub struct RawConsole {
    in_mode: u32,
    out_mode: u32,
    in_cp: u32,
    out_cp: u32,
}

impl RawConsole {
    pub fn enter() -> std::io::Result<RawConsole> {
        unsafe {
            let hin = std_handle(STD_INPUT_HANDLE);
            let hout = std_handle(STD_OUTPUT_HANDLE);
            let mut in_mode = 0;
            let mut out_mode = 0;
            if GetConsoleMode(hin, &mut in_mode) == 0 || GetConsoleMode(hout, &mut out_mode) == 0 {
                return Err(std::io::Error::other("stdin/stdout must be a Windows console"));
            }
            let saved = RawConsole { in_mode, out_mode, in_cp: GetConsoleCP(), out_cp: GetConsoleOutputCP() };

            SetConsoleCP(65001);
            SetConsoleOutputCP(65001);

            // No line editing, echo or Ctrl-C processing: every key goes to the server.
            let raw_in = ENABLE_VIRTUAL_TERMINAL_INPUT
                | ENABLE_WINDOW_INPUT
                | ENABLE_EXTENDED_FLAGS
                | (in_mode & ENABLE_QUICK_EDIT_MODE);
            if SetConsoleMode(hin, raw_in) == 0 {
                return Err(std::io::Error::other("this console does not support virtual terminal input"));
            }
            let vt_out = out_mode | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING | DISABLE_NEWLINE_AUTO_RETURN;
            if SetConsoleMode(hout, vt_out) == 0 {
                SetConsoleMode(hin, in_mode);
                return Err(std::io::Error::other("this console does not support virtual terminal output"));
            }
            Ok(saved)
        }
    }
}

impl Drop for RawConsole {
    fn drop(&mut self) {
        unsafe {
            SetConsoleMode(std_handle(STD_INPUT_HANDLE), self.in_mode);
            SetConsoleMode(std_handle(STD_OUTPUT_HANDLE), self.out_mode);
            SetConsoleCP(self.in_cp);
            SetConsoleOutputCP(self.out_cp);
        }
    }
}

/// True if both stdin and stdout are attached to a console.
pub fn is_console() -> bool {
    unsafe {
        let mut mode = 0;
        GetConsoleMode(std_handle(STD_INPUT_HANDLE), &mut mode) != 0
            && GetConsoleMode(std_handle(STD_OUTPUT_HANDLE), &mut mode) != 0
    }
}

/// Visible window size as (columns, rows).
pub fn window_size() -> Option<(i32, i32)> {
    unsafe {
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
        if GetConsoleScreenBufferInfo(std_handle(STD_OUTPUT_HANDLE), &mut info) == 0 {
            return None;
        }
        let w = (info.srWindow.Right - info.srWindow.Left + 1) as i32;
        let h = (info.srWindow.Bottom - info.srWindow.Top + 1) as i32;
        if w > 0 && h > 0 { Some((w, h)) } else { None }
    }
}

pub fn write_out(s: &str) {
    if s.is_empty() {
        return;
    }
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(s.as_bytes());
    let _ = out.flush();
}

/// Reads console input records on a background thread and forwards the
/// VT byte stream (UTF-8) and resize notifications to the main loop.
pub fn spawn_input_thread(tx: Sender<Event>) {
    std::thread::spawn(move || {
        let hin = std_handle(STD_INPUT_HANDLE);
        let mut records: Vec<INPUT_RECORD> = vec![unsafe { std::mem::zeroed() }; 512];
        let mut pending_high: Option<u16> = None;
        loop {
            let mut n = 0u32;
            let ok = unsafe { ReadConsoleInputW(hin, records.as_mut_ptr(), records.len() as u32, &mut n) };
            if ok == 0 {
                let _ = tx.send(Event::InputEof);
                return;
            }
            let mut units: Vec<u16> = Vec::new();
            let mut resized = false;
            for rec in &records[..n as usize] {
                match rec.EventType as u32 {
                    KEY_EVENT => {
                        let k = unsafe { rec.Event.KeyEvent };
                        let ch = unsafe { k.uChar.UnicodeChar };
                        if k.bKeyDown == 0 || ch == 0 {
                            continue;
                        }
                        for _ in 0..k.wRepeatCount.max(1) {
                            units.push(ch);
                        }
                    }
                    WINDOW_BUFFER_SIZE_EVENT => resized = true,
                    _ => {}
                }
            }
            if !units.is_empty() {
                let mut all = Vec::with_capacity(units.len() + 1);
                if let Some(h) = pending_high.take() {
                    all.push(h);
                }
                all.extend(units);
                // keep an unpaired trailing high surrogate for the next batch
                if let Some(&last) = all.last() {
                    if (0xD800..0xDC00).contains(&last) {
                        pending_high = all.pop();
                    }
                }
                let s = String::from_utf16_lossy(&all);
                if !s.is_empty() && tx.send(Event::Input(s.into_bytes())).is_err() {
                    return;
                }
            }
            if resized && tx.send(Event::Resize).is_err() {
                return;
            }
        }
    });
}

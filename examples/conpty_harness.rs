// SPDX-License-Identifier: GPL-3.0-or-later
// mosh-win: Copyright 2026 the mosh-win authors.
// Ported from mosh (https://mosh.org): Copyright 2012 Keith Winstein and the mosh authors.

//! Test harness: runs a command inside a Windows pseudo console (ConPTY),
//! types scripted input into it, and renders its output with mosh-win's
//! own terminal emulator so the screen can be inspected.
//!
//! Usage: conpty_harness SCRIPT COMMAND-LINE
//! Script lines: "<delay_ms> <text>" (escapes \r \n \e \xNN), "<delay_ms> DUMP",
//! "<delay_ms> RESIZE <cols>x<rows>".

use std::ffi::c_void;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mosh_win::terminal::emulator;
use mosh_win::terminal::framebuffer::Framebuffer;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::Console::*;
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::*;

fn unescape(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut b = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
            continue;
        }
        match chars.next() {
            Some('r') => out.push(b'\r'),
            Some('n') => out.push(b'\n'),
            Some('e') => out.push(0x1b),
            Some('x') => {
                let h: String = chars.by_ref().take(2).collect();
                out.push(u8::from_str_radix(&h, 16).unwrap());
            }
            Some(o) => out.push(o as u8),
            None => {}
        }
    }
    out
}

struct Output {
    bytes: Vec<u8>,
    resizes: Vec<(usize, i32, i32)>,
}

fn render(out: &Output) -> String {
    let mut fb = Framebuffer::new(80, 24);
    let mut pos = 0;
    for &(at, w, h) in &out.resizes {
        emulator::act(&mut fb, &out.bytes[pos..at]);
        fb.resize(w, h);
        pos = at;
    }
    emulator::act(&mut fb, &out.bytes[pos..]);
    let mut text = fb.to_text();
    text.push_str(&format!(
        "[cursor row={} col={} visible={}] [title={}]\n",
        fb.ds.cursor_row(),
        fb.ds.cursor_col(),
        fb.ds.cursor_visible,
        fb.window_title().iter().collect::<String>()
    ));
    text
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let script = std::fs::read_to_string(&args[1]).expect("script");
    let cmdline = args[2..].join(" ");

    unsafe {
        let (mut in_read, mut in_write, mut out_read, mut out_write) =
            (std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
        assert!(CreatePipe(&mut in_read, &mut in_write, std::ptr::null(), 0) != 0);
        assert!(CreatePipe(&mut out_read, &mut out_write, std::ptr::null(), 0) != 0);

        let mut hpc: HPCON = std::mem::zeroed();
        let hr = CreatePseudoConsole(COORD { X: 80, Y: 24 }, in_read, out_write, 0, &mut hpc);
        assert_eq!(hr, 0, "CreatePseudoConsole");

        let mut size = 0usize;
        InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size);
        let mut attr_buf = vec![0u8; size];
        let attrs = attr_buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        assert!(InitializeProcThreadAttributeList(attrs, 1, 0, &mut size) != 0);
        assert!(
            UpdateProcThreadAttribute(
                attrs,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                hpc as *const c_void,
                std::mem::size_of::<HPCON>(),
                std::ptr::null_mut(),
                std::ptr::null(),
            ) != 0
        );

        let mut si: STARTUPINFOEXW = std::mem::zeroed();
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        si.lpAttributeList = attrs;
        // otherwise the child inherits our (redirected) std handles instead of the pseudo console
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let mut wide: Vec<u16> = cmdline.encode_utf16().chain(std::iter::once(0)).collect();
        assert!(
            CreateProcessW(
                std::ptr::null(),
                wide.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                EXTENDED_STARTUPINFO_PRESENT,
                std::ptr::null(),
                std::ptr::null(),
                &si.StartupInfo,
                &mut pi,
            ) != 0,
            "CreateProcessW"
        );
        CloseHandle(in_read);
        CloseHandle(out_write);

        let output = Arc::new(Mutex::new(Output { bytes: Vec::new(), resizes: Vec::new() }));
        let out2 = output.clone();
        let out_read_addr = out_read as usize;
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 65536];
            loop {
                let mut n = 0u32;
                if ReadFile(out_read_addr as HANDLE, buf.as_mut_ptr(), buf.len() as u32, &mut n, std::ptr::null_mut()) == 0
                    || n == 0
                {
                    break;
                }
                out2.lock().unwrap().bytes.extend_from_slice(&buf[..n as usize]);
            }
        });

        for line in script.lines().filter(|l| !l.trim().is_empty()) {
            let (delay, rest) = line.split_once(' ').unwrap_or((line, ""));
            std::thread::sleep(Duration::from_millis(delay.parse().unwrap()));
            if rest == "DUMP" {
                println!("----- screen -----");
                print!("{}", render(&output.lock().unwrap()));
            } else if let Some(sz) = rest.strip_prefix("RESIZE ") {
                let (w, h) = sz.split_once('x').unwrap();
                let (w, h): (i32, i32) = (w.parse().unwrap(), h.parse().unwrap());
                let mut o = output.lock().unwrap();
                let at = o.bytes.len();
                o.resizes.push((at, w, h));
                drop(o);
                ResizePseudoConsole(hpc, COORD { X: w as i16, Y: h as i16 });
            } else {
                let bytes = unescape(rest);
                let mut written = 0u32;
                WriteFile(in_write, bytes.as_ptr(), bytes.len() as u32, &mut written, std::ptr::null_mut());
            }
        }

        let r = WaitForSingleObject(pi.hProcess, 15000);
        let mut code = 0u32;
        GetExitCodeProcess(pi.hProcess, &mut code);
        println!("----- final screen -----");
        print!("{}", render(&output.lock().unwrap()));
        println!("[process {} exit code {}]", if r == WAIT_OBJECT_0 { "exited" } else { "still running" }, code);
        ClosePseudoConsole(hpc);
    }
}

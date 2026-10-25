// SPDX-License-Identifier: GPL-3.0-or-later
// mosh-win: Copyright 2026 the mosh-win authors.
// Ported from mosh (https://mosh.org): Copyright 2012 Keith Winstein and the mosh authors.

pub mod display;
pub mod emulator;
pub mod framebuffer;

#[cfg(test)]
mod tests {
    use super::display::Display;
    use super::emulator::act;
    use super::framebuffer::Framebuffer;

    /// Applying new_frame's output to a copy of the old frame must reproduce the new frame,
    /// which is exactly what the server relies on when it sends us diffs.
    fn roundtrip(w: i32, h: i32, script: &[&[u8]]) {
        let display = Display::new();
        let mut server = Framebuffer::new(w, h);
        let mut client = Framebuffer::new(w, h);
        for step in script {
            let before = server.clone();
            act(&mut server, step);
            let diff = display.new_frame(true, &before, &server);
            act(&mut client, diff.as_bytes());
            assert_eq!(client.to_text(), server.to_text(), "after {:?}", String::from_utf8_lossy(step));
            assert_eq!(client.ds.cursor_row(), server.ds.cursor_row());
            assert_eq!(client.ds.cursor_col(), server.ds.cursor_col());
            for y in 0..h {
                for x in 0..w {
                    // same notion of equality as mosh's Cell::compare (blank == space)
                    let (a, b) = (client.get_cell(y, x), server.get_cell(y, x));
                    let (mut ga, mut gb) = (String::new(), String::new());
                    a.print_grapheme(&mut ga);
                    b.print_grapheme(&mut gb);
                    assert_eq!(ga, gb, "grapheme {y},{x}");
                    assert_eq!(a.renditions(), b.renditions(), "renditions {y},{x}");
                    assert_eq!(a.wide(), b.wide(), "wide {y},{x}");
                    assert_eq!(a.wrap(), b.wrap(), "wrap {y},{x}");
                }
            }
        }
    }

    #[test]
    fn diff_roundtrip() {
        let mut long = Vec::new();
        for i in 0..40 {
            long.extend_from_slice(format!("line {i} \x1b[31mred\x1b[0m 日本語テキスト\r\n").as_bytes());
        }
        roundtrip(
            20,
            6,
            &[
                b"hello world",
                b"\r\nsecond line that is long enough to wrap around the edge",
                b"\x1b[1;1H\x1b[2K\x1b[44m   blue  \x1b[0m",
                &long,
                b"\x1b[2J\x1b[3;5H\x1b[1;4mX\x1b[m",
                "e\u{301}\u{301}x".as_bytes(),
                b"\x1b[38;2;10;20;30mtrue\x1b[48;5;200mcolor",
                b"\x1b]0;title\x07\x1b[?2004h\x1b[?1000h\x1b[?1006h",
                b"\x1b[2;5r\x1b[5;1H\n\n\nscroll\x1b[r",
                b"\x1b[10@ins\x1b[3P",
                "全角が端で折り返す全角が端で".as_bytes(),
            ],
        );
    }
}

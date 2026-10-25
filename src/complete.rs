// SPDX-License-Identifier: GPL-3.0-or-later
// mosh-win: Copyright 2026 the mosh-win authors.
// Ported from mosh (https://mosh.org): Copyright 2012 Keith Winstein and the mosh authors.

//! The remote state we receive: the server's terminal framebuffer plus echo ack
//! (client half of mosh's `Terminal::Complete`).

use prost::Message;

use crate::proto::HostMessage;
use crate::terminal::emulator;
use crate::terminal::framebuffer::Framebuffer;

#[derive(Clone)]
pub struct Complete {
    fb: Framebuffer,
    echo_ack: u64,
}

impl Complete {
    pub fn new(width: i32, height: i32) -> Self {
        Complete { fb: Framebuffer::new(width, height), echo_ack: 0 }
    }

    pub fn fb(&self) -> &Framebuffer {
        &self.fb
    }

    pub fn echo_ack(&self) -> u64 {
        self.echo_ack
    }

    pub fn apply_string(&mut self, diff: &[u8]) {
        let Ok(input) = HostMessage::decode(diff) else { return };
        for inst in input.instruction {
            if let Some(hb) = inst.hostbytes {
                emulator::act(&mut self.fb, &hb.hoststring.unwrap_or_default());
            } else if let Some(r) = inst.resize {
                let (w, h) = (r.width.unwrap_or(0), r.height.unwrap_or(0));
                if w > 0 && h > 0 {
                    self.fb.resize(w, h);
                }
            } else if let Some(e) = inst.echoack {
                let n = e.echo_ack_num.unwrap_or(0);
                if n >= self.echo_ack {
                    self.echo_ack = n;
                }
            }
        }
    }
}

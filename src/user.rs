// SPDX-License-Identifier: GPL-3.0-or-later
// mosh-win: Copyright 2026 the mosh-win authors.
// Ported from mosh (https://mosh.org): Copyright 2012 Keith Winstein and the mosh authors.

//! The client's outgoing state: a stream of keystrokes and resizes
//! (port of mosh's `Network::UserStream`).

use std::collections::VecDeque;

use prost::Message;

use crate::proto::{Keystroke, ResizeMessage, UserInstruction, UserMessage};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UserEvent {
    Byte(u8),
    Resize(i32, i32),
}

#[derive(Clone, PartialEq, Eq, Default, Debug)]
pub struct UserStream {
    actions: VecDeque<UserEvent>,
}

impl UserStream {
    pub fn push_back(&mut self, e: UserEvent) {
        self.actions.push_back(e);
    }

    /// Remove `prefix` (which must be a prefix of this stream).
    pub fn subtract(&mut self, prefix: &UserStream) {
        for e in &prefix.actions {
            debug_assert_eq!(self.actions.front(), Some(e));
            self.actions.pop_front();
        }
    }

    pub fn diff_from(&self, existing: &UserStream) -> Vec<u8> {
        debug_assert!(existing.actions.iter().zip(self.actions.iter()).all(|(a, b)| a == b));
        let mut output = UserMessage::default();
        for e in self.actions.iter().skip(existing.actions.len()) {
            match *e {
                UserEvent::Byte(b) => {
                    if let Some(UserInstruction { keystroke: Some(k), .. }) = output.instruction.last_mut() {
                        k.keys.get_or_insert_with(Vec::new).push(b);
                    } else {
                        output.instruction.push(UserInstruction {
                            keystroke: Some(Keystroke { keys: Some(vec![b]) }),
                            resize: None,
                        });
                    }
                }
                UserEvent::Resize(w, h) => output.instruction.push(UserInstruction {
                    keystroke: None,
                    resize: Some(ResizeMessage { width: Some(w), height: Some(h) }),
                }),
            }
        }
        output.encode_to_vec()
    }
}

// SPDX-License-Identifier: GPL-3.0-or-later
// mosh-win: Copyright 2026 the mosh-win authors.
// Ported from mosh (https://mosh.org): Copyright 2012 Keith Winstein and the mosh authors.

//! Protocol buffer messages, hand-written equivalents of mosh's
//! transportinstruction.proto, userinput.proto and hostinput.proto.
//! (proto2 extensions are encoded on the wire exactly like ordinary fields.)

#[derive(Clone, PartialEq, prost::Message)]
pub struct Instruction {
    #[prost(uint32, optional, tag = "1")]
    pub protocol_version: Option<u32>,
    #[prost(uint64, optional, tag = "2")]
    pub old_num: Option<u64>,
    #[prost(uint64, optional, tag = "3")]
    pub new_num: Option<u64>,
    #[prost(uint64, optional, tag = "4")]
    pub ack_num: Option<u64>,
    #[prost(uint64, optional, tag = "5")]
    pub throwaway_num: Option<u64>,
    #[prost(bytes = "vec", optional, tag = "6")]
    pub diff: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "7")]
    pub chaff: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct UserMessage {
    #[prost(message, repeated, tag = "1")]
    pub instruction: Vec<UserInstruction>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct UserInstruction {
    #[prost(message, optional, tag = "2")]
    pub keystroke: Option<Keystroke>,
    #[prost(message, optional, tag = "3")]
    pub resize: Option<ResizeMessage>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Keystroke {
    #[prost(bytes = "vec", optional, tag = "4")]
    pub keys: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ResizeMessage {
    #[prost(int32, optional, tag = "5")]
    pub width: Option<i32>,
    #[prost(int32, optional, tag = "6")]
    pub height: Option<i32>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct HostMessage {
    #[prost(message, repeated, tag = "1")]
    pub instruction: Vec<HostInstruction>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct HostInstruction {
    #[prost(message, optional, tag = "2")]
    pub hostbytes: Option<HostBytes>,
    #[prost(message, optional, tag = "3")]
    pub resize: Option<ResizeMessage>,
    #[prost(message, optional, tag = "7")]
    pub echoack: Option<EchoAck>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct HostBytes {
    #[prost(bytes = "vec", optional, tag = "4")]
    pub hoststring: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct EchoAck {
    #[prost(uint64, optional, tag = "8")]
    pub echo_ack_num: Option<u64>,
}

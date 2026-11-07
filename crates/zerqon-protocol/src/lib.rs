pub const MINECRAFT_VERSION: &str = "26.3";
pub const PROTOCOL_VERSION: i32 = 777;
mod client;
pub mod codec;
pub mod inbound;
mod teleport;
pub use client::{decode_command, decode_keepalive};
pub use codec::Error as ProtocolError;
pub use teleport::{TeleportAck, decode_teleport_ack, encode_teleport_ack};

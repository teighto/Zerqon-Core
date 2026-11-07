use crate::{
    ProtocolError,
    codec::{get_string, get_varint, take},
};

/// Decode the complete command envelope. Signatures, timestamp freshness and
/// the last-seen message chain still require session-level authentication.
pub fn decode_command(mut body: &[u8], signed: bool) -> Result<&str, ProtocolError> {
    if body.len() > 100_808 {
        return Err(ProtocolError::Invalid("command packet exceeds its limit"));
    }
    let command = get_string(&mut body, 32767)?;
    if signed {
        take(&mut body, 16)?;
        let signatures = get_varint(&mut body)?;
        if !(0..=8).contains(&signatures) {
            return Err(ProtocolError::Invalid("invalid command signature count"));
        }
        for _ in 0..signatures {
            get_string(&mut body, 16)?;
            take(&mut body, 256)?;
        }
        if get_varint(&mut body)? < 0 {
            return Err(ProtocolError::Invalid("negative last-seen offset"));
        }
        let acknowledged = take(&mut body, 3)?;
        if acknowledged[2] & 0xf0 != 0 {
            return Err(ProtocolError::Invalid("last-seen bitset exceeds 20 bits"));
        }
        take(&mut body, 1)?;
    }
    if !body.is_empty() {
        return Err(ProtocolError::Invalid("trailing command bytes"));
    }
    Ok(command)
}

pub fn decode_keepalive(body: &[u8]) -> Result<i64, ProtocolError> {
    Ok(i64::from_be_bytes(body.try_into().map_err(|_| {
        ProtocolError::Invalid("keepalive must contain exactly 8 bytes")
    })?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{put_string, put_varint};

    #[test]
    fn unsigned_command_rejects_trailing_bytes_and_signed_header_is_required() {
        let mut command = Vec::new();
        put_string("stop", &mut command);
        assert_eq!(decode_command(&command, false).unwrap(), "stop");
        assert!(decode_command(&command, true).is_err());
        command.push(0);
        assert!(decode_command(&command, false).is_err());
    }

    #[test]
    fn keepalive_requires_exactly_one_signed_big_endian_long() {
        let bytes = (-0x102030405060708i64).to_be_bytes();
        assert_eq!(decode_keepalive(&bytes).unwrap(), -0x102030405060708);
        assert!(decode_keepalive(&bytes[..7]).is_err());
        let mut extra = bytes.to_vec();
        extra.push(0);
        assert!(decode_keepalive(&extra).is_err());
    }

    fn signed(command: &str) -> Vec<u8> {
        let mut bytes = Vec::new();
        put_string(command, &mut bytes);
        bytes.extend_from_slice(&1794038400123i64.to_be_bytes());
        bytes.extend_from_slice(&(-9i64).to_be_bytes());
        put_varint(1, &mut bytes);
        put_string("message", &mut bytes);
        bytes.extend(0u8..=255);
        put_varint(130, &mut bytes);
        bytes.extend_from_slice(&[1, 1, 8, 219]);
        bytes
    }

    #[test]
    fn signed_command_parses_every_field_and_rejects_every_truncated_prefix() {
        let bytes = signed("say teighto");
        assert_eq!(decode_command(&bytes, true).unwrap(), "say teighto");
        for end in 0..bytes.len() {
            assert!(decode_command(&bytes[..end], true).is_err(), "prefix{end}");
        }
        let mut extra = bytes;
        extra.push(0);
        assert!(decode_command(&extra, true).is_err());
    }

    #[test]
    fn command_and_signature_names_use_java_utf16_limits() {
        for (value, valid) in [
            ("a".repeat(32767), true),
            ("a".repeat(32768), false),
            ("😀".repeat(16383) + "a", true),
            ("😀".repeat(16384), false),
        ] {
            let mut bytes = Vec::new();
            put_string(&value, &mut bytes);
            assert_eq!(decode_command(&bytes, false).is_ok(), valid);
        }
        for (name, valid) in [
            ("a".repeat(16), true),
            ("a".repeat(17), false),
            ("😀".repeat(8), true),
            ("😀".repeat(9), false),
        ] {
            let mut bytes = Vec::new();
            put_string("say x", &mut bytes);
            bytes.extend_from_slice(&[0; 16]);
            put_varint(1, &mut bytes);
            put_string(&name, &mut bytes);
            bytes.extend_from_slice(&[0; 256]);
            bytes.extend_from_slice(&[0; 5]);
            assert_eq!(decode_command(&bytes, true).is_ok(), valid);
        }
    }

    #[test]
    fn signed_argument_count_offset_and_unused_acknowledgement_bits_are_bounded() {
        for (count, valid) in [(0, true), (8, true), (9, false), (-1, false)] {
            let mut bytes = Vec::new();
            put_string("say x", &mut bytes);
            bytes.extend_from_slice(&[0; 16]);
            put_varint(count, &mut bytes);
            for _ in 0..count.max(0) {
                put_string("message", &mut bytes);
                bytes.extend_from_slice(&[0; 256]);
            }
            bytes.extend_from_slice(&[0; 5]);
            assert_eq!(decode_command(&bytes, true).is_ok(), valid);
        }
        let mut prefix = Vec::new();
        put_string("version", &mut prefix);
        prefix.extend_from_slice(&[0; 17]);
        let mut negative = prefix.clone();
        put_varint(-1, &mut negative);
        negative.extend_from_slice(&[0; 4]);
        assert!(decode_command(&negative, true).is_err());
        let mut high_bits = prefix;
        high_bits.extend_from_slice(&[0, 0, 0, 16, 0]);
        assert!(decode_command(&high_bits, true).is_err());
    }
}

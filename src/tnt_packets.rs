use zerqon_protocol::codec::put_varint;

pub fn metadata(id: i32, fuse: u16) -> Vec<u8> {
    let mut packet = vec![101];
    put_varint(id, &mut packet);
    packet.extend_from_slice(&[5, 8, 0, 8, 1]);
    put_varint(i32::from(fuse), &mut packet);
    packet.push(255);
    packet
}

pub fn explosion(position: [f64; 3], blocks: usize) -> Vec<u8> {
    let mut packet = vec![36];
    for value in position {
        packet.extend_from_slice(&value.to_be_bytes());
    }
    packet.extend_from_slice(&4.0f32.to_be_bytes());
    packet.extend_from_slice(&(blocks as i32).to_be_bytes());
    packet.push(0);
    // Verified against the original 26.3 ClientboundExplodePacket codec.
    put_varint(29, &mut packet);
    put_varint(704, &mut packet);
    packet.extend_from_slice(&[0, 1]);
    packet
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tnt_wire_matches_original_26_3_metadata_and_explosion_codec() {
        assert_eq!(metadata(300, 80), [101, 172, 2, 5, 8, 0, 8, 1, 80, 255]);
        let actual: String = explosion([1.5, 65.0, -2.5], 17)[1..]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(
            actual,
            "3ff80000000000004050400000000000c0040000000000004080000000000011001dc0050001"
        );
    }
}

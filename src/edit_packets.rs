use bytes::Bytes;
use std::collections::BTreeMap;
use zerqon_protocol::codec::put_varint;

pub fn section_updates(edits: &[([i32; 3], u32)]) -> Vec<Bytes> {
    let mut sections = BTreeMap::<[i32; 3], Vec<u64>>::new();
    for &([x, y, z], state) in edits {
        let local = ((x.rem_euclid(16) as u64) << 8)
            | ((z.rem_euclid(16) as u64) << 4)
            | y.rem_euclid(16) as u64;
        sections
            .entry([x.div_euclid(16), y.div_euclid(16), z.div_euclid(16)])
            .or_default()
            .push((u64::from(state) << 12) | local);
    }
    sections
        .into_iter()
        .map(|([x, y, z], changes)| {
            let section = ((i64::from(x) & 0x3fffff) << 42)
                | ((i64::from(z) & 0x3fffff) << 20)
                | (i64::from(y) & 0xfffff);
            let mut packet = Vec::with_capacity(14 + changes.len() * 5);
            put_varint(86, &mut packet);
            packet.extend_from_slice(&section.to_be_bytes());
            put_varint(changes.len() as i32, &mut packet);
            for mut value in changes {
                while value >= 0x80 {
                    packet.push((value as u8 & 0x7f) | 0x80);
                    value >>= 7;
                }
                packet.push(value as u8);
            }
            packet.into()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zerqon_protocol::codec::{get_varint, take};

    #[test]
    fn section_updates_preserve_negative_coordinates_and_group_blocks() {
        let packets = section_updates(&[([-1, -1, -1], 25), ([-2, -2, -2], 7), ([0, 16, 0], 8)]);
        assert_eq!(packets.len(), 2);
        let mut first = packets[0].as_ref();
        assert_eq!(get_varint(&mut first).unwrap(), 86);
        assert_eq!(take(&mut first, 8).unwrap(), (-1i64).to_be_bytes());
        assert_eq!(get_varint(&mut first).unwrap(), 2);
        assert_eq!(get_varint(&mut first).unwrap(), (25 << 12) | 4095);
        assert_eq!(get_varint(&mut first).unwrap(), (7 << 12) | 0xeee);
        assert!(first.is_empty());
        let mut second = packets[1].as_ref();
        assert_eq!(get_varint(&mut second).unwrap(), 86);
        assert_eq!(take(&mut second, 8).unwrap(), 1i64.to_be_bytes());
        assert_eq!(get_varint(&mut second).unwrap(), 1);
        assert_eq!(get_varint(&mut second).unwrap(), 8 << 12);
        assert!(second.is_empty());
    }
}

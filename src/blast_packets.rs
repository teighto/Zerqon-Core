use crate::game_mode::GameMode;
use zerqon_protocol::codec::{put_string, put_varint};

pub fn death(entity_id: i32) -> Vec<u8> {
    combat_kill(entity_id, "Killed by an explosion")
}

pub(crate) fn combat_kill(entity_id: i32, message: &str) -> Vec<u8> {
    let mut packet = vec![69];
    put_varint(entity_id, &mut packet);
    crate::packets::text_component(message, &mut packet);
    packet
}

pub fn respawn(
    dimension_type: i32,
    mode: GameMode,
    previous: Option<GameMode>,
    flat: bool,
) -> Vec<u8> {
    let mut packet = vec![84];
    put_varint(dimension_type, &mut packet);
    put_string("minecraft:overworld", &mut packet);
    packet.extend_from_slice(&0i64.to_be_bytes());
    put_varint(i32::from(mode), &mut packet);
    put_varint(
        previous.map_or(0, |value| i32::from(value) + 1),
        &mut packet,
    );
    packet.extend_from_slice(&[0, u8::from(flat), 0, 0, 62, 3]);
    packet
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn death_and_respawn_match_original_26_3_codec() {
        assert_eq!(combat_kill(300, "TNT"), [69, 172, 2, 8, 0, 3, 84, 78, 84]);
        let encoded: String = respawn(0, GameMode::Survival, None, false)[1..]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(
            encoded,
            "00136d696e6563726166743a6f766572776f726c6400000000000000000000000000003e03"
        );
    }
}

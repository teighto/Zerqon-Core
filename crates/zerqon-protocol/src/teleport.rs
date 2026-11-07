use crate::{
    ProtocolError,
    codec::{get_varint, put_varint, take},
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TeleportAck {
    pub id: i32,
    pub xyz: [f64; 3],
    /// Yaw, then pitch, in degrees.
    pub rotation: [f32; 2],
}

/// Decode the complete 26.3 packet body, excluding its packet ID and frame.
/// Zerqon rejects nonpositive IDs, nonfinite values and positions outside its
/// horizontal ±30,000,000 / vertical ±20,000,000 limits instead of clamping them.
pub fn decode_teleport_ack(mut body: &[u8]) -> Result<TeleportAck, ProtocolError> {
    let id = get_varint(&mut body)?;
    if body.len() != 32 {
        return Err(ProtocolError::Invalid(
            "teleport acknowledgement requires position and rotation",
        ));
    }
    let mut xyz = [0.0; 3];
    for value in &mut xyz {
        let mut bytes = [0; 8];
        bytes.copy_from_slice(take(&mut body, 8)?);
        *value = f64::from_be_bytes(bytes);
    }
    let mut rotation = [0.0; 2];
    for value in &mut rotation {
        let mut bytes = [0; 4];
        bytes.copy_from_slice(take(&mut body, 4)?);
        *value = f32::from_be_bytes(bytes);
    }
    let ack = TeleportAck { id, xyz, rotation };
    validate(&ack)?;
    Ok(ack)
}

/// Encode a validated acknowledgement body in the same format as the 26.3 client.
pub fn encode_teleport_ack(ack: &TeleportAck) -> Result<Vec<u8>, ProtocolError> {
    validate(ack)?;
    let mut body = Vec::with_capacity(37);
    put_varint(ack.id, &mut body);
    for value in ack.xyz {
        body.extend_from_slice(&value.to_be_bytes());
    }
    for value in ack.rotation {
        body.extend_from_slice(&value.to_be_bytes());
    }
    Ok(body)
}

fn validate(ack: &TeleportAck) -> Result<(), ProtocolError> {
    if ack.id <= 0
        || ack.xyz.iter().any(|value| !value.is_finite())
        || ack.rotation.iter().any(|value| !value.is_finite())
        || ack.xyz[0].abs() > 30_000_000.0
        || ack.xyz[1].abs() > 20_000_000.0
        || ack.xyz[2].abs() > 30_000_000.0
    {
        return Err(ProtocolError::Invalid(
            "invalid teleport acknowledgement values",
        ));
    }
    Ok(())
}

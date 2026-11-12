use crate::codec::put_varint;
use crate::codec::{Error, get_varint, take};

pub fn get_lp_vec3(input: &mut &[u8]) -> Result<[f64; 3], Error> {
    let first = take(input, 1)?[0];
    if first == 0 {
        return Ok([0.0; 3]);
    }
    let second = take(input, 1)?[0];
    let high = u32::from_be_bytes(take(input, 4)?.try_into().expect("four bytes"));
    let bits = u64::from(first) | (u64::from(second) << 8) | (u64::from(high) << 16);
    let mut scale = u64::from(first & 3);
    if first & 4 != 0 {
        scale |= u64::from(get_varint(input)? as u32) << 2;
    }
    Ok([3, 18, 33]
        .map(|shift| (((bits >> shift) & 32767) as f64 / 32766.0 * 2.0 - 1.0) * scale as f64))
}

const MAX_VALUE: f64 = 17_179_869_183.0;
const MIN_VALUE: f64 = 1.0 / 32766.0;

/// Append Minecraft 26.3's lossy LP Vec3 encoding. Like the original codec,
/// NaN becomes zero and infinities/large values clamp to ±(2^34 - 1).
/// Authoritative callers must validate physics state before encoding it.
pub fn put_lp_vec3(vector: [f64; 3], output: &mut Vec<u8>) {
    let vector = vector.map(|value| {
        if value.is_nan() {
            0.0
        } else {
            value.clamp(-MAX_VALUE, MAX_VALUE)
        }
    });
    let magnitude = vector
        .iter()
        .map(|value| value.abs())
        .fold(0.0_f64, f64::max);
    if magnitude < MIN_VALUE {
        output.push(0);
        return;
    }
    let scale = magnitude.ceil() as u64;
    let extended = scale > 3;
    let header = (scale & 3) | if extended { 4 } else { 0 };
    let quantized =
        vector.map(|value| ((value / scale as f64 * 0.5 + 0.5) * 32766.0).round() as u64);
    let packed = header | (quantized[0] << 3) | (quantized[1] << 18) | (quantized[2] << 33);
    output.push(packed as u8);
    output.push((packed >> 8) as u8);
    output.extend_from_slice(&((packed >> 16) as u32).to_be_bytes());
    if extended {
        put_varint((scale >> 2) as u32 as i32, output);
    }
}

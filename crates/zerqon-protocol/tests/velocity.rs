use proptest::prelude::*;
use zerqon_protocol::{
    codec::{get_varint, take},
    put_lp_vec3,
};

fn bits(text: &str) -> f64 {
    f64::from_bits(u64::from_str_radix(text, 16).unwrap())
}

fn hex(text: &str) -> Vec<u8> {
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

fn reference_decode(mut body: &[u8]) -> [f64; 3] {
    let first = take(&mut body, 1).unwrap()[0];
    if first == 0 {
        assert!(body.is_empty());
        return [0.0; 3];
    }
    let second = take(&mut body, 1).unwrap()[0];
    let upper = u32::from_be_bytes(take(&mut body, 4).unwrap().try_into().unwrap());
    let packed = (u64::from(upper) << 16) | (u64::from(second) << 8) | u64::from(first);
    let mut scale = u64::from(first & 3);
    if first & 4 != 0 {
        scale |= u64::from(get_varint(&mut body).unwrap() as u32) << 2;
    }
    assert!(body.is_empty());
    [3, 18, 33].map(|shift| {
        let value = ((packed >> shift) & 32767).min(32766) as f64;
        (value * 2.0 / 32766.0 - 1.0) * scale as f64
    })
}

#[test]
fn every_vector_matches_the_original_26_3_codec_bytes_and_decoded_bits() {
    let mut checked = 0;
    for line in include_str!("../../../tools/entity-wire/26.3.tsv")
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
    {
        let fields: Vec<_> = line.split('\t').collect();
        assert_eq!(fields.len(), 8);
        let input = [bits(fields[1]), bits(fields[2]), bits(fields[3])];
        let expected = hex(fields[4]);
        let mut encoded = Vec::new();
        put_lp_vec3(input, &mut encoded);
        assert_eq!(encoded, expected, "{}", fields[0]);
        let decoded = reference_decode(&encoded).map(f64::to_bits);
        let original = [bits(fields[5]), bits(fields[6]), bits(fields[7])].map(f64::to_bits);
        assert_eq!(decoded, original, "{}", fields[0]);
        checked += 1;
    }
    assert_eq!(checked, 143);
}

#[test]
fn zero_threshold_and_extended_unsigned_scale_keep_the_original_boundaries() {
    let threshold = 1.0 / 32766.0_f64;
    let mut before = Vec::new();
    put_lp_vec3(
        [threshold.next_down(), -threshold.next_down(), -0.0],
        &mut before,
    );
    assert_eq!(before, [0]);
    let mut at = Vec::new();
    put_lp_vec3([threshold, 0.0, 0.0], &mut at);
    assert_eq!(at.len(), 6);
    assert_ne!(at[0], 0);
    let mut maximum = Vec::new();
    put_lp_vec3([17_179_869_183.0, -17_179_869_183.0, 0.0], &mut maximum);
    assert_eq!(maximum.len(), 11);
    assert_eq!(&maximum[6..], &[255, 255, 255, 255, 15]);
    assert_eq!(
        reference_decode(&maximum),
        [17_179_869_183.0, -17_179_869_183.0, 0.0]
    );
}

fn check_loss(input: [f64; 3]) -> Result<(), TestCaseError> {
    let mut bytes = vec![9, 8, 7];
    put_lp_vec3(input, &mut bytes);
    prop_assert_eq!(&bytes[..3], &[9, 8, 7]);
    prop_assert!((1..=11).contains(&(bytes.len() - 3)));
    let decoded = reference_decode(&bytes[3..]);
    let sanitized = input.map(|value| {
        if value.is_nan() {
            0.0
        } else {
            value.clamp(-17_179_869_183.0, 17_179_869_183.0)
        }
    });
    let scale = sanitized
        .iter()
        .map(|value| value.abs())
        .fold(0.0_f64, f64::max)
        .ceil()
        .max(1.0);
    let tolerance = scale / 32766.0 + scale * f64::EPSILON * 4.0;
    for axis in 0..3 {
        prop_assert!(decoded[axis].is_finite());
        prop_assert!(decoded[axis].abs() <= 17_179_869_183.0);
        prop_assert!((decoded[axis] - sanitized[axis]).abs() <= tolerance);
    }
    Ok(())
}

proptest! {
    #[test]
    fn all_ieee_inputs_have_bounded_output_and_original_quantization_error(raw in prop::array::uniform3(any::<u64>())) {
        check_loss(raw.map(f64::from_bits))?;
    }

    #[test]
    fn ordinary_velocities_preserve_all_axes_within_quantization_error(input in prop::array::uniform3(-1000.0_f64..1000.0)) {
        check_loss(input)?;
    }
}

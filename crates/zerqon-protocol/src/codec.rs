use std::io::{self, Read, Write};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O: {0}")]
    Io(#[from] io::Error),
    #[error("invalid protocol data: {0}")]
    Invalid(&'static str),
}

pub fn put_varint(value: i32, out: &mut Vec<u8>) {
    let mut bits = value as u32;
    loop {
        let byte = (bits & 0x7f) as u8;
        bits >>= 7;
        out.push(byte | if bits != 0 { 0x80 } else { 0 });
        if bits == 0 {
            break;
        }
    }
}

pub fn get_varint(data: &mut &[u8]) -> Result<i32, Error> {
    let mut value = 0u32;
    for index in 0..5 {
        let byte = take(data, 1)?[0];
        if index == 4 && byte & 0xf0 != 0 {
            return Err(Error::Invalid("VarInt overflow"));
        }
        value |= u32::from(byte & 0x7f) << (7 * index);
        if byte & 0x80 == 0 {
            return Ok(value as i32);
        }
    }
    Err(Error::Invalid("VarInt too long"))
}

pub fn take<'a>(data: &mut &'a [u8], len: usize) -> Result<&'a [u8], Error> {
    if len > data.len() {
        return Err(Error::Invalid("truncated packet"));
    }
    let (head, tail) = data.split_at(len);
    *data = tail;
    Ok(head)
}

pub fn put_string(value: &str, out: &mut Vec<u8>) {
    put_varint(value.len() as i32, out);
    out.extend_from_slice(value.as_bytes());
}

pub fn get_string<'a>(data: &mut &'a [u8], max_chars: usize) -> Result<&'a str, Error> {
    let len =
        usize::try_from(get_varint(data)?).map_err(|_| Error::Invalid("negative string length"))?;
    if len > max_chars.saturating_mul(3) {
        return Err(Error::Invalid("string too long"));
    }
    let value =
        std::str::from_utf8(take(data, len)?).map_err(|_| Error::Invalid("invalid UTF-8"))?;
    if value.encode_utf16().count() > max_chars {
        return Err(Error::Invalid("string too long"));
    }
    Ok(value)
}

pub async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    max_len: usize,
    compression: Option<usize>,
) -> Result<Vec<u8>, Error> {
    let mut len = 0usize;
    let mut finished = false;
    for index in 0..3 {
        let byte = reader.read_u8().await?;
        len |= usize::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            finished = true;
            break;
        }
    }
    if !finished || len == 0 || len > max_len {
        return Err(Error::Invalid("invalid frame size"));
    }
    let mut frame = vec![0; len];
    reader.read_exact(&mut frame).await?;
    let Some(threshold) = compression else {
        return Ok(frame);
    };
    let mut payload = frame.as_slice();
    let expected = usize::try_from(get_varint(&mut payload)?)
        .map_err(|_| Error::Invalid("negative decompressed size"))?;
    if expected == 0 {
        if payload.is_empty() || payload.len() >= threshold {
            return Err(Error::Invalid("uncompressed packet above threshold"));
        }
        return Ok(payload.to_vec());
    }
    if expected < threshold || expected > max_len {
        return Err(Error::Invalid("invalid decompressed size"));
    }
    let mut decoder = flate2::read::ZlibDecoder::new(payload);
    let mut result = Vec::with_capacity(expected);
    decoder
        .by_ref()
        .take(expected as u64 + 1)
        .read_to_end(&mut result)?;
    if result.len() != expected || decoder.total_in() as usize != payload.len() {
        return Err(Error::Invalid("compression length mismatch"));
    }
    Ok(result)
}

pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    payload: &[u8],
    compression: Option<usize>,
) -> Result<(), Error> {
    if payload.is_empty() || payload.len() > 2_097_151 {
        return Err(Error::Invalid("invalid frame size"));
    }
    let mut framed = Vec::new();
    let body = match compression {
        None => payload,
        Some(threshold) => {
            if payload.len() >= threshold {
                put_varint(payload.len() as i32, &mut framed);
                let mut encoder =
                    flate2::write::ZlibEncoder::new(&mut framed, flate2::Compression::fast());
                encoder.write_all(payload)?;
                encoder.finish()?;
            } else {
                framed.push(0);
                framed.extend_from_slice(payload);
            }
            framed.as_slice()
        }
    };
    if body.len() > 2_097_151 {
        return Err(Error::Invalid("frame too large"));
    }
    let mut prefix = Vec::with_capacity(3);
    put_varint(body.len() as i32, &mut prefix);
    writer.write_all(&prefix).await?;
    writer.write_all(body).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn varints_match_wire_fixtures() {
        for (value, wire) in [
            (0, vec![0]),
            (127, vec![127]),
            (128, vec![128, 1]),
            (777, vec![137, 6]),
            (-1, vec![255, 255, 255, 255, 15]),
            (i32::MIN, vec![128, 128, 128, 128, 8]),
        ] {
            let mut out = Vec::new();
            put_varint(value, &mut out);
            assert_eq!(out, wire);
            assert_eq!(get_varint(&mut wire.as_slice()).unwrap(), value);
        }
    }

    #[test]
    fn rejects_varint_overflow_and_truncation() {
        for bad in [
            &[128][..],
            &[255, 255, 255, 255, 127],
            &[128, 128, 128, 128, 128, 0],
        ] {
            assert!(get_varint(&mut &bad[..]).is_err());
        }
    }

    #[test]
    fn bounds_unicode_in_utf16_units() {
        let wire = [4, 240, 159, 152, 128];
        assert_eq!(get_string(&mut wire.as_slice(), 2).unwrap(), "😀");
        assert!(get_string(&mut wire.as_slice(), 1).is_err());
        assert!(get_string(&mut &[1, 255][..], 10).is_err());
        assert!(get_string(&mut &[8, b'a'][..], 10).is_err());
    }

    #[tokio::test]
    async fn bounded_frame_rejects_oversize_before_body() {
        let mut reader = &[127][..];
        assert!(matches!(
            read_frame(&mut reader, 16, None).await,
            Err(Error::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn framing_preserves_packet_with_and_without_compression() {
        for threshold in [None, Some(256), Some(0)] {
            let payload = vec![5; 4096];
            let mut wire = Vec::new();
            write_frame(&mut wire, &payload, threshold).await.unwrap();
            let read = read_frame(&mut wire.as_slice(), 8192, threshold)
                .await
                .unwrap();
            assert_eq!(read, payload);
        }
    }

    proptest! {
        #[test]
        fn signed_varint_roundtrip(value in any::<i32>()) {
            let mut out = Vec::new();
            put_varint(value, &mut out);
            prop_assert!(out.len() <= 5);
            let mut slice = out.as_slice();
            prop_assert_eq!(get_varint(&mut slice).unwrap(), value);
            prop_assert!(slice.is_empty());
        }
    }
}

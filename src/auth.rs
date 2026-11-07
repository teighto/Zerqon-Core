use aes::cipher::KeyIvInit;
use aws_lc_rs::{
    constant_time::verify_slices_are_equal,
    digest::{self, SHA1_FOR_LEGACY_USE_ONLY},
    encoding::{AsDer, PublicKeyX509Der},
    rand::{SecureRandom, SystemRandom},
    rsa::{KeySize, Pkcs1PrivateDecryptingKey, PrivateDecryptingKey},
};
use serde::Deserialize;
use std::{
    io,
    net::IpAddr,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use uuid::Uuid;
use zeroize::Zeroizing;

const SESSION_ENDPOINT: &str = "https://sessionserver.mojang.com/session/minecraft/hasJoined";
const MAX_PROFILE_BYTES: usize = 65_536;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("authentication cryptography could not be initialized")]
    CryptoInitialization,
    #[error("invalid encryption response")]
    InvalidEncryption,
    #[error("invalid login username")]
    InvalidUsername,
    #[error("session authentication was rejected")]
    Rejected,
    #[error("session service returned an invalid profile")]
    InvalidProfile,
    #[error("session service is unavailable")]
    Unavailable,
}

pub struct SharedSecret(Zeroizing<[u8; 16]>);

impl SharedSecret {
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct ProfileProperty {
    pub name: String,
    pub value: String,
    pub signature: Option<String>,
}

#[derive(Debug)]
pub struct VerifiedProfile {
    pub uuid: Uuid,
    pub name: String,
    pub properties: Vec<ProfileProperty>,
}

pub struct Authenticator {
    private_key: Pkcs1PrivateDecryptingKey,
    public_key: Vec<u8>,
    client: reqwest::Client,
}

impl Authenticator {
    pub async fn new() -> Result<Self, AuthError> {
        let (private_key, public_key) = tokio::task::spawn_blocking(|| {
            let private = PrivateDecryptingKey::generate(KeySize::Rsa2048)
                .map_err(|_| AuthError::CryptoInitialization)?;
            let public = AsDer::<PublicKeyX509Der>::as_der(&private.public_key())
                .map_err(|_| AuthError::CryptoInitialization)?
                .as_ref()
                .to_vec();
            let private = Pkcs1PrivateDecryptingKey::new(private)
                .map_err(|_| AuthError::CryptoInitialization)?;
            Ok::<_, AuthError>((private, public))
        })
        .await
        .map_err(|_| AuthError::CryptoInitialization)??;
        Ok(Self {
            private_key,
            public_key,
            client: session_client()?,
        })
    }
    pub fn public_key_der(&self) -> &[u8] {
        &self.public_key
    }
    pub fn verify_response(
        &self,
        secret: &[u8],
        token: &[u8],
        expected: &[u8; 4],
    ) -> Result<SharedSecret, AuthError> {
        let length = self.private_key.key_size_bytes();
        if secret.len() != length || token.len() != length || length > 512 {
            return Err(AuthError::InvalidEncryption);
        }
        let mut plaintext_secret = Zeroizing::new(vec![0; self.private_key.min_output_size()]);
        let mut plaintext_token = Zeroizing::new(vec![0; self.private_key.min_output_size()]);
        let secret_length = self
            .private_key
            .decrypt(secret, &mut plaintext_secret)
            .map(|data| data.len());
        let token_length = self
            .private_key
            .decrypt(token, &mut plaintext_token)
            .map(|data| data.len());
        let token_matches = verify_slices_are_equal(&plaintext_token[..4], expected).is_ok();
        if secret_length != Ok(16) || token_length != Ok(4) || !token_matches {
            return Err(AuthError::InvalidEncryption);
        }
        let mut result = Zeroizing::new([0; 16]);
        result.copy_from_slice(&plaintext_secret[..16]);
        Ok(SharedSecret(result))
    }
    pub async fn authenticate(
        &self,
        name: &str,
        hash: &str,
        ip: Option<IpAddr>,
    ) -> Result<VerifiedProfile, AuthError> {
        request_profile(&self.client, SESSION_ENDPOINT, name, hash, ip).await
    }
}

pub fn fresh_verify_token() -> Result<[u8; 4], AuthError> {
    let mut token = [0; 4];
    SystemRandom::new()
        .fill(&mut token)
        .map_err(|_| AuthError::CryptoInitialization)?;
    Ok(token)
}

pub fn session_hash(secret: &SharedSecret, public_key: &[u8]) -> String {
    let mut context = digest::Context::new(&SHA1_FOR_LEGACY_USE_ONLY);
    context.update(secret.as_bytes());
    context.update(public_key);
    let hash = context.finish();
    let mut bytes = [0; 20];
    bytes.copy_from_slice(hash.as_ref());
    signed_hex(&bytes)
}

fn session_client() -> Result<reqwest::Client, AuthError> {
    reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("Zerqon/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| AuthError::Unavailable)
}

async fn request_profile(
    client: &reqwest::Client,
    endpoint: &str,
    name: &str,
    hash: &str,
    ip: Option<IpAddr>,
) -> Result<VerifiedProfile, AuthError> {
    if !valid_username(name) {
        return Err(AuthError::InvalidUsername);
    }
    let unsigned_hash = hash.strip_prefix('-').unwrap_or(hash);
    if unsigned_hash.is_empty()
        || unsigned_hash.len() > 40
        || !unsigned_hash.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(AuthError::Rejected);
    }
    let mut request = client
        .get(endpoint)
        .query(&[("username", name), ("serverId", hash)]);
    if let Some(ip) = ip {
        request = request.query(&[("ip", ip.to_string())]);
    }
    let mut response = request.send().await.map_err(|_| AuthError::Unavailable)?;
    match response.status() {
        reqwest::StatusCode::OK => {}
        reqwest::StatusCode::NO_CONTENT | reqwest::StatusCode::FORBIDDEN => {
            return Err(AuthError::Rejected);
        }
        _ => return Err(AuthError::Unavailable),
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PROFILE_BYTES as u64)
    {
        return Err(AuthError::InvalidProfile);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| AuthError::Unavailable)? {
        if chunk.len() > MAX_PROFILE_BYTES - bytes.len() {
            return Err(AuthError::InvalidProfile);
        }
        bytes.extend_from_slice(&chunk);
    }
    parse_profile(&bytes, name)
}

fn parse_profile(bytes: &[u8], requested_name: &str) -> Result<VerifiedProfile, AuthError> {
    #[derive(Deserialize)]
    struct Response {
        id: String,
        name: String,
        #[serde(default)]
        properties: Vec<ProfileProperty>,
    }
    if bytes.len() > MAX_PROFILE_BYTES {
        return Err(AuthError::InvalidProfile);
    }
    let response: Response =
        serde_json::from_slice(bytes).map_err(|_| AuthError::InvalidProfile)?;
    if !valid_username(&response.name)
        || !response.name.eq_ignore_ascii_case(requested_name)
        || response.id.len() != 32
        || !response.id.bytes().all(|byte| byte.is_ascii_hexdigit())
        || response.properties.len() > 32
        || response.properties.iter().any(|property| {
            property.name.is_empty()
                || property.name.len() > 64
                || property.value.len() > 8192
                || property
                    .signature
                    .as_ref()
                    .is_some_and(|signature| signature.len() > 4096)
        })
    {
        return Err(AuthError::InvalidProfile);
    }
    let uuid = Uuid::parse_str(&response.id).map_err(|_| AuthError::InvalidProfile)?;
    if uuid.is_nil() {
        return Err(AuthError::InvalidProfile);
    }
    Ok(VerifiedProfile {
        uuid,
        name: response.name,
        properties: response.properties,
    })
}

fn valid_username(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 16
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

pub struct CipherReader<R> {
    inner: R,
    cipher: cfb8::Decryptor<aes::Aes128>,
}

impl<R> CipherReader<R> {
    pub fn new(inner: R, secret: &SharedSecret) -> Self {
        Self {
            inner,
            cipher: cfb8::Decryptor::new(secret.as_bytes().into(), secret.as_bytes().into()),
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for CipherReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let filled = buf.filled().len();
        match Pin::new(&mut this.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                this.cipher.decrypt(&mut buf.filled_mut()[filled..]);
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

pub struct CipherWriter<W> {
    inner: W,
    cipher: cfb8::Encryptor<aes::Aes128>,
    pending: Vec<u8>,
    offset: usize,
}

impl<W> CipherWriter<W> {
    pub fn new(inner: W, secret: &SharedSecret) -> Self {
        Self {
            inner,
            cipher: cfb8::Encryptor::new(secret.as_bytes().into(), secret.as_bytes().into()),
            pending: Vec::new(),
            offset: 0,
        }
    }
}

impl<W: AsyncWrite + Unpin> CipherWriter<W> {
    fn poll_pending(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while self.offset < self.pending.len() {
            match Pin::new(&mut self.inner).poll_write(cx, &self.pending[self.offset..]) {
                Poll::Ready(Ok(0)) => return Poll::Ready(Err(io::ErrorKind::WriteZero.into())),
                Poll::Ready(Ok(written)) => self.offset += written,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Pending => return Poll::Pending,
            }
        }
        self.pending.clear();
        self.offset = 0;
        Poll::Ready(Ok(()))
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for CipherWriter<W> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        match this.poll_pending(cx) {
            Poll::Ready(Ok(())) => {}
            Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
            Poll::Pending => return Poll::Pending,
        }
        let length = data.len().min(8192);
        this.pending.extend_from_slice(&data[..length]);
        this.cipher.encrypt(&mut this.pending);
        Poll::Ready(Ok(length))
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match this.poll_pending(cx) {
            Poll::Ready(Ok(())) => Pin::new(&mut this.inner).poll_flush(cx),
            other => other,
        }
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.as_mut().poll_flush(cx) {
            Poll::Ready(Ok(())) => Pin::new(&mut self.get_mut().inner).poll_shutdown(cx),
            other => other,
        }
    }
}

fn signed_hex(bytes: &[u8; 20]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let negative = bytes[0] & 0x80 != 0;
    let mut magnitude = *bytes;
    if negative {
        let mut carry = true;
        for byte in magnitude.iter_mut().rev() {
            let (value, overflow) = (!*byte).overflowing_add(u8::from(carry));
            *byte = value;
            carry = overflow;
        }
    }
    let mut output = String::with_capacity(40);
    for byte in magnitude {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 15)]));
    }
    let trimmed = output.trim_start_matches('0');
    if trimmed.is_empty() {
        "0".to_owned()
    } else if negative {
        format!("-{trimmed}")
    } else {
        trimmed.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::rsa::{Pkcs1PublicEncryptingKey, PublicEncryptingKey};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn secret() -> SharedSecret {
        SharedSecret(Zeroizing::new([0x42; 16]))
    }

    fn encrypt_rsa(public: &[u8], input: &[u8]) -> Vec<u8> {
        let key =
            Pkcs1PublicEncryptingKey::new(PublicEncryptingKey::from_der(public).unwrap()).unwrap();
        let mut output = vec![0; key.ciphertext_size()];
        key.encrypt(input, &mut output).unwrap().to_vec()
    }

    #[tokio::test]
    async fn rsa_response_recovers_secret_and_verifies_challenge() {
        let auth = Authenticator::new().await.unwrap();
        let token = fresh_verify_token().unwrap();
        let encrypted_secret = encrypt_rsa(auth.public_key_der(), &[0x42; 16]);
        let encrypted_token = encrypt_rsa(auth.public_key_der(), &token);
        let recovered = auth
            .verify_response(&encrypted_secret, &encrypted_token, &token)
            .unwrap();
        assert_eq!(recovered.as_bytes(), &[0x42; 16]);
        let wrong_token = encrypt_rsa(auth.public_key_der(), &[5; 4]);
        assert!(matches!(
            auth.verify_response(&encrypted_secret, &wrong_token, &[6; 4]),
            Err(AuthError::InvalidEncryption)
        ));
        for bad_secret in [vec![], vec![0; 15], vec![0; 17]] {
            let ciphertext = encrypt_rsa(auth.public_key_der(), &bad_secret);
            assert!(matches!(
                auth.verify_response(&ciphertext, &encrypted_token, &token),
                Err(AuthError::InvalidEncryption)
            ));
        }
        for bad_wire in [vec![], vec![0; 128], vec![0; 256], vec![0; 513]] {
            assert!(matches!(
                auth.verify_response(&bad_wire, &encrypted_token, &token),
                Err(AuthError::InvalidEncryption)
            ));
            assert!(matches!(
                auth.verify_response(&encrypted_secret, &bad_wire, &token),
                Err(AuthError::InvalidEncryption)
            ));
        }
    }

    #[test]
    fn rejects_names_that_could_confuse_session_lookup() {
        for name in [
            "",
            "seventeenletters1",
            "hello world",
            "user&serverId=x",
            "Юзер",
            "a/b",
            "x\0y",
        ] {
            assert!(!valid_username(name), "accepted {name:?}");
        }
        for name in ["a", "Player_123", "sixteenletters12"] {
            assert!(valid_username(name));
        }
    }

    #[test]
    fn session_profile_uses_only_matching_authoritative_identity() {
        let body = br#"{"id":"069a79f444e94726a5befca90e38aaf5","name":"Notch","properties":[{"name":"textures","value":"encoded","signature":"signature"}]}"#;
        let profile = parse_profile(body, "notch").unwrap();
        assert_eq!(
            profile.uuid.to_string(),
            "069a79f4-44e9-4726-a5be-fca90e38aaf5"
        );
        assert_eq!(profile.name, "Notch");
        assert_eq!(profile.properties[0].value, "encoded");
        assert!(parse_profile(body, "Impostor").is_err());
        for bad in [
            br#"{"id":"00000000000000000000000000000000","name":"Notch"}"#.as_slice(),
            br#"{"id":"069a79f4-44e9-4726-a5be-fca90e38aaf5","name":"Notch"}"#,
            br#"{"id":"garbage","name":"Notch"}"#,
            br#"{"id":"069a79f444e94726a5befca90e38aaf5","name":"Bad Name"}"#,
            b"not json",
        ] {
            assert!(parse_profile(bad, "Notch").is_err());
        }
    }

    async fn mock_response(response: Vec<u8>) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://{}/session/minecraft/hasJoined",
            listener.local_addr().unwrap()
        );
        let task = tokio::spawn(async move {
            let (mut stream, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                assert!(request.len() < 4096);
                request.push(stream.read_u8().await.unwrap());
            }
            stream.write_all(&response).await.unwrap();
            String::from_utf8(request).unwrap()
        });
        (url, task)
    }

    fn response(status: &str, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn test_client() -> reqwest::Client {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn session_request_checks_response_and_transmits_bound_hash() {
        let (url, server) = mock_response(response(
            "200 OK",
            r#"{"id":"069a79f444e94726a5befca90e38aaf5","name":"Notch"}"#,
        ))
        .await;
        let profile = request_profile(
            &test_client(),
            &url,
            "Notch",
            "-deadbeef",
            Some("127.0.0.1".parse().unwrap()),
        )
        .await
        .unwrap();
        assert_eq!(profile.name, "Notch");
        let request = server.await.unwrap();
        assert!(request.contains("username=Notch&serverId=-deadbeef&ip=127.0.0.1"));
    }

    #[tokio::test]
    async fn session_service_fails_closed_on_denial_malformed_and_oversized_responses() {
        let oversized = format!(
            r#"{{"id":"069a79f444e94726a5befca90e38aaf5","name":"Notch"}}{}"#,
            " ".repeat(MAX_PROFILE_BYTES)
        );
        for (status, body) in [
            ("204 No Content", String::new()),
            ("403 Forbidden", String::new()),
            ("503 Unavailable", String::new()),
            ("302 Found", String::new()),
            ("200 OK", "broken".to_owned()),
            ("200 OK", oversized),
        ] {
            let (url, server) = mock_response(response(status, &body)).await;
            assert!(
                request_profile(&test_client(), &url, "Notch", "1234", None)
                    .await
                    .is_err()
            );
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn production_session_client_rejects_plain_http() {
        assert!(
            session_client()
                .unwrap()
                .get("http://127.0.0.1:1")
                .send()
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn cfb8_stream_matches_reference_ciphertext_across_write_calls() {
        let key = secret();
        let mut bytes = Vec::new();
        let mut writer = CipherWriter::new(&mut bytes, &key);
        writer.write_all(b"hello ").await.unwrap();
        writer.write_all(b"world").await.unwrap();
        writer.flush().await.unwrap();
        drop(writer);
        assert_eq!(
            bytes,
            [
                0x3e, 0xf8, 0x79, 0xf4, 0x0c, 0xb7, 0xc4, 0x2b, 0x76, 0x6b, 0xcd
            ]
        );
    }

    #[tokio::test]
    async fn cfb8_reader_does_not_reset_between_reads_or_decrypt_existing_data() {
        let wire = [
            0x3e, 0xf8, 0x79, 0xf4, 0x0c, 0xb7, 0xc4, 0x2b, 0x76, 0x6b, 0xcd,
        ];
        let key = secret();
        let mut reader = CipherReader::new(wire.as_slice(), &key);
        let mut first = [0; 3];
        reader.read_exact(&mut first).await.unwrap();
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).await.unwrap();
        assert_eq!(&first, b"hel");
        assert_eq!(rest, b"lo world");
    }

    #[tokio::test]
    async fn reader_preserves_prefilled_read_buffer() {
        let wire = [
            0x3e, 0xf8, 0x79, 0xf4, 0x0c, 0xb7, 0xc4, 0x2b, 0x76, 0x6b, 0xcd,
        ];
        let key = secret();
        let mut reader = CipherReader::new(wire.as_slice(), &key);
        let mut storage = [0; 32];
        let mut buffer = ReadBuf::new(&mut storage);
        buffer.put_slice(b"prefix:");
        std::future::poll_fn(|cx| Pin::new(&mut reader).poll_read(cx, &mut buffer))
            .await
            .unwrap();
        assert_eq!(buffer.filled(), b"prefix:hello world");
    }

    #[derive(Default)]
    struct ShortWriter {
        bytes: Vec<u8>,
        pending: bool,
    }

    impl AsyncWrite for ShortWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            data: &[u8],
        ) -> Poll<io::Result<usize>> {
            if self.pending {
                self.pending = false;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            let count = data.len().min(3);
            self.bytes.extend_from_slice(&data[..count]);
            self.pending = true;
            Poll::Ready(Ok(count))
        }
        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn cancelled_pending_write_preserves_cipher_state_and_unsent_tail() {
        let key = secret();
        let mut sink = ShortWriter::default();
        let mut writer = CipherWriter::new(&mut sink, &key);
        writer.write_all(b"hello ").await.unwrap();
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(
            Pin::new(&mut writer)
                .poll_write(&mut cx, b"world")
                .is_pending()
        );
        writer.write_all(b"world").await.unwrap();
        writer.shutdown().await.unwrap();
        drop(writer);
        assert_eq!(
            sink.bytes,
            [
                0x3e, 0xf8, 0x79, 0xf4, 0x0c, 0xb7, 0xc4, 0x2b, 0x76, 0x6b, 0xcd
            ]
        );
    }

    #[tokio::test]
    async fn encrypted_writer_streams_large_input_through_backpressure() {
        let payload: Vec<u8> = (0..20_000).map(|index| (index % 251) as u8).collect();
        let (sink, mut source) = tokio::io::duplex(7);
        let expected = payload.clone();
        let sending = tokio::spawn(async move {
            let key = secret();
            let mut writer = CipherWriter::new(sink, &key);
            for chunk in payload.chunks(73) {
                writer.write_all(chunk).await.unwrap();
            }
            writer.shutdown().await.unwrap();
        });
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), source.read_to_end(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        sending.await.unwrap();
        assert_eq!(bytes.len(), expected.len());
        cfb8::Decryptor::<aes::Aes128>::new((&[0x42; 16]).into(), (&[0x42; 16]).into())
            .decrypt(&mut bytes);
        assert_eq!(bytes, expected);
    }

    #[tokio::test]
    async fn session_rejects_chunked_overflow_truncation_and_slow_responses() {
        let overflow = format!(
            r#"{{"id":"069a79f444e94726a5befca90e38aaf5","name":"Notch"}}{}"#,
            " ".repeat(MAX_PROFILE_BYTES)
        );
        let chunked = format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{overflow}\r\n", overflow.len()).into_bytes();
        let (url, server) = mock_response(chunked).await;
        assert!(matches!(
            request_profile(&test_client(), &url, "Notch", "1234", None).await,
            Err(AuthError::InvalidProfile)
        ));
        server.await.unwrap();
        let truncated =
            b"HTTP/1.1 200 OK\r\nContent-Length: 500\r\nConnection: close\r\n\r\n{}".to_vec();
        let (url, server) = mock_response(truncated).await;
        assert!(matches!(
            request_profile(&test_client(), &url, "Notch", "1234", None).await,
            Err(AuthError::Unavailable)
        ));
        server.await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let slow_client = reqwest::Client::builder()
            .timeout(Duration::from_millis(50))
            .build()
            .unwrap();
        assert!(matches!(
            request_profile(&slow_client, &url, "Notch", "1234", None).await,
            Err(AuthError::Unavailable)
        ));
    }

    #[test]
    fn minecraft_sha1_uses_signed_java_big_integer_format() {
        for (input, expected) in [
            ("Notch", "4ed1f46bbe04bc756bcb17c0c7ce3e4632f06a48"),
            ("jeb_", "-7c9d5b0044c130109a5d7b5fb5c317c02b4e28c1"),
            ("simon", "88e16a1019277b15d58faf0541e11910eb756f6"),
        ] {
            let hash = digest::digest(&SHA1_FOR_LEGACY_USE_ONLY, input.as_bytes());
            assert_eq!(signed_hex(hash.as_ref().try_into().unwrap()), expected);
        }
    }

    #[test]
    fn signed_hash_handles_zero_minus_one_and_minimum_value() {
        assert_eq!(signed_hex(&[0; 20]), "0");
        assert_eq!(signed_hex(&[255; 20]), "-1");
        let mut minimum = [0; 20];
        minimum[0] = 128;
        assert_eq!(
            signed_hex(&minimum),
            "-8000000000000000000000000000000000000000"
        );
    }
}

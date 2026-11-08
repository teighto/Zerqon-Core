use std::time::Duration;

use semver::Version;
use serde::Deserialize;
use tokio::sync::watch;
use tokio::task::JoinHandle;

const ENDPOINT: &str = "https://api.github.com/repos/teighto/Zerqon-Core/releases/latest";
const RELEASE_PREFIX: &str = "https://github.com/teighto/Zerqon-Core/releases/tag/";
const BODY_LIMIT: usize = 65_536;
const MIN_INTERVAL: Duration = Duration::from_secs(3600);
const MAX_INTERVAL: Duration = Duration::from_secs(7 * 24 * 3600);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateAvailable {
    pub version: String,
    pub url: String,
}

impl UpdateAvailable {
    pub fn notification(&self) -> String {
        format!(
            "A new Zerqon version {} is available. Download: {}",
            self.version, self.url
        )
    }
}

#[derive(Clone, Default)]
pub struct NotificationState {
    announced: Option<Version>,
}

impl NotificationState {
    pub fn next(&mut self, operator: bool, update: Option<&UpdateAvailable>) -> Option<String> {
        if !operator {
            return None;
        }
        let update = update?;
        let version = Version::parse(&update.version).ok()?;
        if self
            .announced
            .as_ref()
            .is_some_and(|previous| !version.cmp_precedence(previous).is_gt())
        {
            return None;
        }
        self.announced = Some(version);
        Some(update.notification())
    }
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum UpdateError {
    #[error("invalid current version")]
    InvalidCurrentVersion,
    #[error("invalid update-check interval")]
    InvalidInterval,
    #[error("update request failed")]
    Transport,
    #[error("GitHub update checking is rate limited")]
    RateLimited { retry_after: Option<Duration> },
    #[error("GitHub update endpoint returned HTTP {0}")]
    HttpStatus(u16),
    #[error("update response exceeds 64 KiB")]
    BodyTooLarge,
    #[error("invalid release metadata")]
    InvalidMetadata,
    #[error("release URL does not match the Zerqon repository and tag")]
    InvalidReleaseUrl,
}

pub struct UpdateChecker {
    current: Version,
    client: reqwest::Client,
    endpoint: String,
}

impl UpdateChecker {
    pub fn new(current_version: &str) -> Result<Self, UpdateError> {
        let current =
            Version::parse(current_version).map_err(|_| UpdateError::InvalidCurrentVersion)?;
        let client = client_builder()
            .https_only(true)
            .build()
            .map_err(|_| UpdateError::Transport)?;
        Ok(Self {
            current,
            client,
            endpoint: ENDPOINT.to_owned(),
        })
    }

    pub async fn check(&self) -> Result<Option<UpdateAvailable>, UpdateError> {
        let mut response = self
            .client
            .get(&self.endpoint)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2026-03-10")
            .send()
            .await
            .map_err(|_| UpdateError::Transport)?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || (status == reqwest::StatusCode::FORBIDDEN
                && (response
                    .headers()
                    .get("x-ratelimit-remaining")
                    .is_some_and(|value| value == "0")
                    || response
                        .headers()
                        .contains_key(reqwest::header::RETRY_AFTER)))
        {
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .map(|seconds| Duration::from_secs(seconds.min(24 * 3600)));
            return Err(UpdateError::RateLimited { retry_after });
        }
        if status != reqwest::StatusCode::OK {
            return Err(UpdateError::HttpStatus(status.as_u16()));
        }
        if response
            .content_length()
            .is_some_and(|size| size > BODY_LIMIT as u64)
        {
            return Err(UpdateError::BodyTooLarge);
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| UpdateError::Transport)? {
            if chunk.len() > BODY_LIMIT.saturating_sub(body.len()) {
                return Err(UpdateError::BodyTooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        parse_release(&body, &self.current)
    }

    pub fn spawn(self, interval: Duration) -> Result<UpdateMonitor, UpdateError> {
        if !(MIN_INTERVAL..=MAX_INTERVAL).contains(&interval) {
            return Err(UpdateError::InvalidInterval);
        }
        let (sender, receiver) = watch::channel(None);
        let task = tokio::spawn(async move { self.run(interval, sender).await });
        Ok(UpdateMonitor {
            receiver,
            task: Some(task),
        })
    }

    async fn run(self, interval: Duration, sender: watch::Sender<Option<UpdateAvailable>>) {
        let mut failures = 0_u32;
        loop {
            let delay = match self.check().await {
                Ok(update) => {
                    failures = 0;
                    sender.send_if_modified(|previous| {
                        if previous == &update {
                            return false;
                        }
                        *previous = update;
                        true
                    });
                    interval
                }
                Err(error) => {
                    if failures == 0 {
                        tracing::warn!(%error, "Zerqon update check failed; retrying in the background");
                    }
                    failures = failures.saturating_add(1);
                    retry_delay(interval, failures, &error)
                }
            };
            tokio::time::sleep(delay).await;
        }
    }

    #[cfg(test)]
    fn with_endpoint(current: &str, endpoint: String, timeout: Duration) -> Self {
        Self {
            current: Version::parse(current).unwrap(),
            client: client_builder().timeout(timeout).build().unwrap(),
            endpoint,
        }
    }
}

pub struct UpdateMonitor {
    receiver: watch::Receiver<Option<UpdateAvailable>>,
    task: Option<JoinHandle<()>>,
}

impl UpdateMonitor {
    pub fn subscribe(&self) -> watch::Receiver<Option<UpdateAvailable>> {
        self.receiver.clone()
    }
    pub async fn stop(mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for UpdateMonitor {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .referer(false)
        .no_proxy()
        .pool_max_idle_per_host(1)
        .user_agent(concat!(
            "Zerqon/",
            env!("CARGO_PKG_VERSION"),
            " (+https://github.com/teighto/Zerqon-Core)"
        ))
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    draft: bool,
    prerelease: bool,
}

fn parse_release(body: &[u8], current: &Version) -> Result<Option<UpdateAvailable>, UpdateError> {
    if body.len() > BODY_LIMIT {
        return Err(UpdateError::BodyTooLarge);
    }
    let release: Release =
        serde_json::from_slice(body).map_err(|_| UpdateError::InvalidMetadata)?;
    if release.draft || release.prerelease {
        return Ok(None);
    }
    if release.tag_name.len() > 128 {
        return Err(UpdateError::InvalidMetadata);
    }
    let version = Version::parse(
        release
            .tag_name
            .strip_prefix('v')
            .unwrap_or(&release.tag_name),
    )
    .map_err(|_| UpdateError::InvalidMetadata)?;
    if !version.pre.is_empty() || !version.cmp_precedence(current).is_gt() {
        return Ok(None);
    }
    let expected_url = format!("{RELEASE_PREFIX}{}", release.tag_name);
    if release.html_url != expected_url {
        return Err(UpdateError::InvalidReleaseUrl);
    }
    Ok(Some(UpdateAvailable {
        version: version.to_string(),
        url: release.html_url,
    }))
}

fn retry_delay(interval: Duration, failures: u32, error: &UpdateError) -> Duration {
    let multiplier = 1_u64 << failures.saturating_sub(1).min(10);
    let base = Duration::from_secs(300 * multiplier).min(interval);
    match error {
        UpdateError::RateLimited {
            retry_after: Some(delay),
        } => base.max(*delay),
        _ => base,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const RELEASE: &[u8] = br#"{"tag_name":"v0.0.2","html_url":"https://github.com/teighto/Zerqon-Core/releases/tag/v0.0.2","draft":false,"prerelease":false}"#;

    #[test]
    fn notifications_require_operator_and_do_not_consume_unprivileged_attempts() {
        let update = UpdateAvailable {
            version: "0.0.2".into(),
            url: format!("{RELEASE_PREFIX}v0.0.2"),
        };
        let mut state = NotificationState::default();
        assert!(state.next(false, Some(&update)).is_none());
        assert_eq!(state.next(true, Some(&update)), Some(update.notification()));
        assert!(state.next(true, Some(&update)).is_none());
    }

    #[test]
    fn new_session_receives_update_but_same_session_only_announces_newer_versions() {
        let first = UpdateAvailable {
            version: "0.0.2".into(),
            url: format!("{RELEASE_PREFIX}v0.0.2"),
        };
        let second = UpdateAvailable {
            version: "0.0.3".into(),
            url: format!("{RELEASE_PREFIX}v0.0.3"),
        };
        let mut state = NotificationState::default();
        assert!(state.next(true, Some(&first)).is_some());
        assert!(state.next(true, None).is_none());
        assert!(state.next(false, Some(&second)).is_none());
        assert!(state.next(true, Some(&second)).is_some());
        assert!(state.next(true, Some(&first)).is_none());
        assert!(
            NotificationState::default()
                .next(true, Some(&second))
                .is_some()
        );
    }

    #[test]
    fn stable_new_release_produces_english_notification() {
        let update = parse_release(RELEASE, &Version::parse("0.0.1").unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            update.notification(),
            "A new Zerqon version 0.0.2 is available. Download: https://github.com/teighto/Zerqon-Core/releases/tag/v0.0.2"
        );
    }

    #[test]
    fn invalid_current_version_is_rejected() {
        assert!(UpdateChecker::new("not-a-version").is_err());
    }

    fn release(tag: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "tag_name": tag,
            "html_url": format!("{RELEASE_PREFIX}{tag}"),
            "draft": false,
            "prerelease": false
        }))
        .unwrap()
    }

    #[test]
    fn comparison_uses_semver_precedence_and_ignores_build_metadata() {
        for (current, tag, expected) in [
            ("0.0.2", "v0.0.2", false),
            ("0.0.3", "v0.0.2", false),
            ("0.9.9", "v0.10.0", true),
            ("0.0.2-alpha.1", "0.0.2", true),
            ("0.0.2+abc", "v0.0.2+def", false),
            ("0.0.1", "v0.0.2-rc.1", false),
        ] {
            assert_eq!(
                parse_release(&release(tag), &Version::parse(current).unwrap())
                    .unwrap()
                    .is_some(),
                expected,
                "{current} -> {tag}"
            );
        }
    }

    #[test]
    fn draft_and_prerelease_responses_are_not_announced() {
        for field in ["draft", "prerelease"] {
            let mut data: serde_json::Value = serde_json::from_slice(RELEASE).unwrap();
            data[field] = true.into();
            assert!(
                parse_release(&serde_json::to_vec(&data).unwrap(), &Version::new(0, 0, 1))
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn malformed_release_metadata_is_rejected() {
        for body in [b"not JSON".as_slice(), b"{}", br#"{"draft":false,"prerelease":false,"tag_name":"v0.0.2"}"#, br#"{"draft":"false","prerelease":false,"tag_name":"v0.0.2","html_url":"https://github.com"}"#] {
            assert_eq!(parse_release(body, &Version::new(0,0,1)), Err(UpdateError::InvalidMetadata));
        }
        assert_eq!(
            parse_release(&release("latest"), &Version::new(0, 0, 1)),
            Err(UpdateError::InvalidMetadata)
        );
    }

    #[test]
    fn release_link_must_match_repository_and_tag_exactly() {
        for url in [
            "https://evil.example/teighto/Zerqon-Core/releases/tag/v0.0.2",
            "http://github.com/teighto/Zerqon-Core/releases/tag/v0.0.2",
            "https://github.com.evil.example/teighto/Zerqon-Core/releases/tag/v0.0.2",
            "https://evil.example@github.com/teighto/Zerqon-Core/releases/tag/v0.0.2",
            "https://github.com/teighto/Other/releases/tag/v0.0.2",
            "https://github.com/teighto/Zerqon-Core/releases/tag/v0.0.3",
            "https://github.com/teighto/Zerqon-Core/releases/tag/v0.0.2?redirect=https://evil.example",
            "https://github.com/teighto/Zerqon-Core/releases/tag/v0.0.2#fragment",
            "https://github.com/teighto/Zerqon-Core/releases/tag/../v0.0.2",
        ] {
            let mut data: serde_json::Value = serde_json::from_slice(RELEASE).unwrap();
            data["html_url"] = url.into();
            assert_eq!(
                parse_release(&serde_json::to_vec(&data).unwrap(), &Version::new(0, 0, 1)),
                Err(UpdateError::InvalidReleaseUrl),
                "{url}"
            );
        }
    }

    async fn serve(status: &str, headers: &str, body: &[u8]) -> (String, JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/latest", listener.local_addr().unwrap());
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
            body.len()
        );
        let body = body.to_vec();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(socket.read_u8().await.unwrap());
                assert!(request.len() <= 16384);
            }
            socket.write_all(response.as_bytes()).await.unwrap();
            let _ = socket.write_all(&body).await;
            String::from_utf8(request).unwrap()
        });
        (endpoint, task)
    }

    #[tokio::test]
    async fn fetches_release_without_credentials_and_uses_expected_headers() {
        let (endpoint, task) = serve("200 OK", "Content-Type: application/json\r\n", RELEASE).await;
        let checker = UpdateChecker::with_endpoint("0.0.1", endpoint, Duration::from_secs(2));
        assert_eq!(checker.check().await.unwrap().unwrap().version, "0.0.2");
        let request = task.await.unwrap().to_ascii_lowercase();
        assert!(request.starts_with("get /latest http/1.1\r\n"));
        assert!(request.contains("user-agent: zerqon/"));
        assert!(request.contains("accept: application/vnd.github+json"));
        assert!(!request.contains("authorization:"));
        assert!(!request.contains("cookie:"));
    }

    #[tokio::test]
    async fn no_published_release_is_normal() {
        let (endpoint, task) = serve("404 Not Found", "", b"{}").await;
        let checker = UpdateChecker::with_endpoint("0.0.1", endpoint, Duration::from_secs(2));
        assert_eq!(checker.check().await.unwrap(), None);
        task.await.unwrap();
    }

    #[tokio::test]
    async fn service_error_and_rate_limits_are_reported_without_body_leaks() {
        for (status, headers, expected) in [
            (
                "500 Internal Server Error",
                "",
                UpdateError::HttpStatus(500),
            ),
            (
                "403 Forbidden",
                "x-ratelimit-remaining: 0\r\nRetry-After: 600\r\n",
                UpdateError::RateLimited {
                    retry_after: Some(Duration::from_secs(600)),
                },
            ),
            (
                "429 Too Many Requests",
                "Retry-After: 999999999\r\n",
                UpdateError::RateLimited {
                    retry_after: Some(Duration::from_secs(86400)),
                },
            ),
        ] {
            let (endpoint, task) = serve(status, headers, b"private error details").await;
            let checker = UpdateChecker::with_endpoint("0.0.1", endpoint, Duration::from_secs(2));
            let error = checker.check().await.unwrap_err();
            assert_eq!(error, expected);
            assert!(!error.to_string().contains("private"));
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn redirects_are_never_followed() {
        let destination = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let headers = format!(
            "Location: http://{}/do-not-connect\r\n",
            destination.local_addr().unwrap()
        );
        let (endpoint, task) = serve("302 Found", &headers, &[]).await;
        let checker = UpdateChecker::with_endpoint("0.0.1", endpoint, Duration::from_secs(2));
        assert_eq!(checker.check().await, Err(UpdateError::HttpStatus(302)));
        assert!(
            tokio::time::timeout(Duration::from_millis(30), destination.accept())
                .await
                .is_err()
        );
        task.await.unwrap();
    }

    #[tokio::test]
    async fn oversized_content_length_is_rejected() {
        let (endpoint, task) = serve("200 OK", "", &vec![b' '; BODY_LIMIT + 1]).await;
        let checker = UpdateChecker::with_endpoint("0.0.1", endpoint, Duration::from_secs(2));
        assert_eq!(checker.check().await, Err(UpdateError::BodyTooLarge));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn oversized_chunked_body_is_rejected() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/latest", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut data = Vec::new();
            while !data.ends_with(b"\r\n\r\n") {
                data.push(socket.read_u8().await.unwrap());
            }
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            for _ in 0..17 {
                if socket.write_all(b"1000\r\n").await.is_err() {
                    break;
                }
                if socket.write_all(&vec![b' '; 4096]).await.is_err() {
                    break;
                }
                if socket.write_all(b"\r\n").await.is_err() {
                    break;
                }
            }
            let _ = socket.write_all(b"0\r\n\r\n").await;
        });
        let checker = UpdateChecker::with_endpoint("0.0.1", endpoint, Duration::from_secs(2));
        assert_eq!(checker.check().await, Err(UpdateError::BodyTooLarge));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn stalled_endpoint_times_out() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/latest", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        let checker = UpdateChecker::with_endpoint("0.0.1", endpoint, Duration::from_millis(50));
        assert_eq!(checker.check().await, Err(UpdateError::Transport));
        task.abort();
    }

    #[tokio::test]
    async fn monitor_checks_immediately_and_can_be_stopped() {
        let (endpoint, task) = serve("200 OK", "", RELEASE).await;
        let checker = UpdateChecker::with_endpoint("0.0.1", endpoint, Duration::from_secs(2));
        let monitor = checker.spawn(Duration::from_secs(6 * 3600)).unwrap();
        let mut receiver = monitor.subscribe();
        tokio::time::timeout(Duration::from_secs(2), receiver.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            receiver.borrow_and_update().as_ref().unwrap().version,
            "0.0.2"
        );
        monitor.stop().await;
        assert!(receiver.changed().await.is_err());
        task.await.unwrap();
    }

    #[tokio::test]
    async fn monitor_deduplicates_same_release_and_retains_it_on_transient_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/latest", listener.local_addr().unwrap());
        let (completed, received) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            for status in ["200 OK", "200 OK", "500 Internal Server Error"] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    request.push(socket.read_u8().await.unwrap());
                }
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    RELEASE.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.write_all(RELEASE).await.unwrap();
            }
            completed.send(()).unwrap();
        });
        let checker = UpdateChecker::with_endpoint("0.0.1", endpoint, Duration::from_secs(2));
        let (sender, mut receiver) = watch::channel(None);
        let background = tokio::spawn(checker.run(Duration::from_millis(10), sender));
        tokio::time::timeout(Duration::from_secs(2), receiver.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            receiver.borrow_and_update().as_ref().unwrap().version,
            "0.0.2"
        );
        tokio::time::timeout(Duration::from_secs(2), received)
            .await
            .unwrap()
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), receiver.changed())
                .await
                .is_err()
        );
        assert_eq!(receiver.borrow().as_ref().unwrap().version, "0.0.2");
        background.abort();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn dropping_monitor_closes_watch_channel() {
        let (endpoint, task) = serve("200 OK", "", RELEASE).await;
        let checker = UpdateChecker::with_endpoint("0.0.1", endpoint, Duration::from_secs(2));
        let monitor = checker.spawn(MIN_INTERVAL).unwrap();
        let mut receiver = monitor.subscribe();
        tokio::time::timeout(Duration::from_secs(2), receiver.changed())
            .await
            .unwrap()
            .unwrap();
        receiver.borrow_and_update();
        drop(monitor);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), receiver.changed())
                .await
                .unwrap()
                .is_err()
        );
        task.await.unwrap();
    }

    #[tokio::test]
    async fn interval_validation_prevents_accidental_polling_storm() {
        for interval in [
            Duration::ZERO,
            Duration::from_secs(3599),
            Duration::from_secs(7 * 24 * 3600 + 1),
        ] {
            assert!(matches!(
                UpdateChecker::new("0.0.1").unwrap().spawn(interval),
                Err(UpdateError::InvalidInterval)
            ));
        }
    }

    #[test]
    fn backoff_grows_and_honors_rate_limit_retry_after() {
        let normal = Duration::from_secs(6 * 3600);
        assert_eq!(
            retry_delay(normal, 1, &UpdateError::Transport),
            Duration::from_secs(300)
        );
        assert_eq!(
            retry_delay(normal, 2, &UpdateError::Transport),
            Duration::from_secs(600)
        );
        assert_eq!(
            retry_delay(normal, u32::MAX, &UpdateError::Transport),
            normal
        );
        assert_eq!(
            retry_delay(
                normal,
                1,
                &UpdateError::RateLimited {
                    retry_after: Some(Duration::from_secs(7200))
                }
            ),
            Duration::from_secs(7200)
        );
    }
}

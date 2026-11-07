use std::{
    collections::HashSet,
    time::{Duration, Instant},
};
use zerqon_protocol::codec::{get_string, get_varint, take};

pub const PROBE_PACKET: &[u8] = b"\x01\x10horizon:mod_list\x00";
const CHANNEL: &str = "horizon:mod_list";
const MAX_PAYLOAD_BYTES: usize = 65_536;
const MAX_ENTRIES: i32 = 128;
const MAX_TEXT: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientProfile {
    JavaCompatible,
    Horizon,
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("invalid client capability response: {0}")]
    Invalid(&'static str),
    #[error("client profile negotiation timed out")]
    TimedOut,
    #[error("client profile negotiation is closed")]
    Closed,
    #[error("duplicate client capability response")]
    Duplicate,
    #[error("Horizon shared content mods are not supported")]
    UnsupportedContent,
    #[error("the selected client profile is disabled")]
    Disabled(ClientProfile),
    #[error("client connections are disabled")]
    AllDisabled,
    #[error(transparent)]
    Wire(#[from] zerqon_protocol::ProtocolError),
}

impl PolicyError {
    pub fn disconnect_reason(&self) -> &'static str {
        match self {
            Self::TimedOut => "Client capability negotiation timed out.",
            Self::UnsupportedContent => "Horizon content mods are not supported by this server.",
            Self::Disabled(ClientProfile::JavaCompatible) => {
                "Java-compatible client connections are disabled on this server."
            }
            Self::Disabled(ClientProfile::Horizon) => {
                "Horizon client connections are disabled on this server."
            }
            Self::AllDisabled => "Client connections are disabled on this server.",
            _ => "Invalid client capability response.",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Waiting,
    Finished,
    Failed,
}

pub struct Negotiation {
    allow_java: bool,
    allow_horizon: bool,
    horizon: bool,
    deadline: Instant,
    phase: Phase,
}

impl Negotiation {
    pub fn new(
        allow_java: bool,
        allow_horizon: bool,
        now: Instant,
        timeout: Duration,
    ) -> Result<Self, PolicyError> {
        if timeout.is_zero() || timeout > Duration::from_secs(300) {
            return Err(PolicyError::Invalid("timeout must be within 300 seconds"));
        }
        let deadline = now
            .checked_add(timeout)
            .ok_or(PolicyError::Invalid("deadline overflow"))?;
        Ok(Self {
            allow_java,
            allow_horizon,
            horizon: false,
            deadline,
            phase: Phase::Waiting,
        })
    }

    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Body of configuration serverbound CustomPayload, after its packet ID.
    pub fn observe_custom_payload(
        &mut self,
        body: &[u8],
        now: Instant,
    ) -> Result<bool, PolicyError> {
        let result = self.observe(body, now);
        if result.is_err() {
            self.phase = Phase::Failed;
        }
        result
    }

    fn observe(&mut self, mut body: &[u8], now: Instant) -> Result<bool, PolicyError> {
        if self.phase == Phase::Failed {
            return Err(PolicyError::Closed);
        }
        if self.phase == Phase::Waiting && now >= self.deadline {
            return Err(PolicyError::TimedOut);
        }
        if body.len() > MAX_PAYLOAD_BYTES {
            return Err(PolicyError::Invalid("payload exceeds 64 KiB"));
        }
        if get_string(&mut body, 32767)? != CHANNEL {
            return Ok(false);
        }
        if self.phase != Phase::Waiting {
            return Err(PolicyError::Closed);
        }
        if self.horizon {
            return Err(PolicyError::Duplicate);
        }
        let entries = parse_list(body)?;
        if entries != 0 {
            return Err(PolicyError::UnsupportedContent);
        }
        self.horizon = true;
        Ok(true)
    }

    /// Freeze the profile at the ordered known-packs response; no extra wait is needed.
    pub fn finish(&mut self, now: Instant) -> Result<ClientProfile, PolicyError> {
        if self.phase != Phase::Waiting {
            return Err(PolicyError::Closed);
        }
        if now >= self.deadline {
            self.phase = Phase::Failed;
            return Err(PolicyError::TimedOut);
        }
        self.phase = Phase::Finished;
        if !self.allow_java && !self.allow_horizon {
            return Err(PolicyError::AllDisabled);
        }
        let profile = if self.horizon {
            ClientProfile::Horizon
        } else {
            ClientProfile::JavaCompatible
        };
        if match profile {
            ClientProfile::JavaCompatible => self.allow_java,
            ClientProfile::Horizon => self.allow_horizon,
        } {
            Ok(profile)
        } else {
            Err(PolicyError::Disabled(profile))
        }
    }
}

fn parse_list(mut bytes: &[u8]) -> Result<usize, PolicyError> {
    let count = get_varint(&mut bytes)?;
    if !(0..=MAX_ENTRIES).contains(&count) {
        return Err(PolicyError::Invalid("too many content entries"));
    }
    let mut ids = HashSet::new();
    for _ in 0..count {
        let id = get_string(&mut bytes, MAX_TEXT)?;
        let version = get_string(&mut bytes, MAX_TEXT)?;
        if id.is_empty() || version.is_empty() || !ids.insert(id) {
            return Err(PolicyError::Invalid("empty or repeated content identifier"));
        }
        if take(&mut bytes, 1)?[0] > 2 {
            return Err(PolicyError::Invalid("unknown content side"));
        }
        match take(&mut bytes, 1)?[0] {
            0 => {}
            1 => {
                take(&mut bytes, 32)?;
            }
            _ => return Err(PolicyError::Invalid("invalid content hash flag")),
        }
    }
    if !bytes.is_empty() {
        return Err(PolicyError::Invalid("trailing content bytes"));
    }
    Ok(count as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zerqon_protocol::codec::{put_string, put_varint};

    fn payload(channel: &str, data: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        put_string(channel, &mut body);
        body.extend_from_slice(data);
        body
    }
    fn policy(java: bool, horizon: bool, now: Instant) -> Negotiation {
        Negotiation::new(java, horizon, now, Duration::from_secs(30)).unwrap()
    }
    fn entry(id: &str, version: &str, side: u8, hash: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        put_string(id, &mut bytes);
        put_string(version, &mut bytes);
        bytes.extend([side, u8::from(hash)]);
        if hash {
            bytes.extend([42; 32]);
        }
        bytes
    }

    #[test]
    fn independent_flags_apply_to_negotiated_profiles_and_both_false_closes_admission() {
        let now = Instant::now();
        for (java, horizon) in [(true, true), (true, false), (false, true), (false, false)] {
            for advertised in [false, true] {
                let mut negotiation = policy(java, horizon, now);
                if advertised {
                    assert!(
                        negotiation
                            .observe_custom_payload(&payload(CHANNEL, &[0]), now)
                            .unwrap()
                    );
                }
                let result = negotiation.finish(now);
                if !java && !horizon {
                    assert!(matches!(result, Err(PolicyError::AllDisabled)));
                } else {
                    let expected = if advertised {
                        ClientProfile::Horizon
                    } else {
                        ClientProfile::JavaCompatible
                    };
                    let allowed = if advertised { horizon } else { java };
                    if allowed {
                        assert_eq!(result.unwrap(), expected);
                    } else {
                        assert!(
                            matches!(result, Err(PolicyError::Disabled(profile)) if profile == expected)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn brand_and_unknown_channels_do_not_claim_horizon_or_add_a_wait() {
        let now = Instant::now();
        let mut negotiation = policy(true, false, now);
        for (channel, bytes) in [
            ("minecraft:brand", b"\x07horizon".as_slice()),
            ("zerqon:other", &[255][..]),
        ] {
            assert!(
                !negotiation
                    .observe_custom_payload(&payload(channel, bytes), now)
                    .unwrap()
            );
        }
        assert_eq!(
            negotiation.finish(now).unwrap(),
            ClientProfile::JavaCompatible
        );
        assert!(
            !negotiation
                .observe_custom_payload(
                    &payload("minecraft:brand", b"\x07vanilla"),
                    now + Duration::from_secs(60)
                )
                .unwrap()
        );
    }

    #[test]
    fn duplicate_or_late_capability_answers_cannot_change_the_decision() {
        let now = Instant::now();
        for repeated in [vec![0], vec![1]] {
            let mut negotiation = policy(true, true, now);
            negotiation
                .observe_custom_payload(&payload(CHANNEL, &[0]), now)
                .unwrap();
            assert!(matches!(
                negotiation.observe_custom_payload(&payload(CHANNEL, &repeated), now),
                Err(PolicyError::Duplicate)
            ));
            assert!(matches!(negotiation.finish(now), Err(PolicyError::Closed)));
        }
        let mut negotiation = policy(true, true, now);
        assert_eq!(
            negotiation.finish(now).unwrap(),
            ClientProfile::JavaCompatible
        );
        assert!(matches!(
            negotiation.observe_custom_payload(&payload(CHANNEL, &[0]), now),
            Err(PolicyError::Closed)
        ));
    }

    #[test]
    fn absolute_deadline_cannot_be_extended_with_unrelated_payloads() {
        let now = Instant::now();
        let mut negotiation = policy(true, true, now);
        let deadline = negotiation.deadline();
        negotiation
            .observe_custom_payload(
                &payload("minecraft:brand", b"\x07vanilla"),
                deadline - Duration::from_nanos(1),
            )
            .unwrap();
        assert!(matches!(
            negotiation.finish(deadline),
            Err(PolicyError::TimedOut)
        ));
        assert!(matches!(
            negotiation.observe_custom_payload(&payload(CHANNEL, &[0]), now),
            Err(PolicyError::Closed)
        ));
        let mut negotiation = policy(true, true, now);
        assert!(matches!(
            negotiation.observe_custom_payload(&payload("minecraft:brand", &[0]), deadline),
            Err(PolicyError::TimedOut)
        ));
        assert!(Negotiation::new(true, true, now, Duration::ZERO).is_err());
        assert!(Negotiation::new(true, true, now, Duration::from_secs(301)).is_err());
    }

    #[test]
    fn valid_nonempty_content_list_is_rejected_without_enabling_mods() {
        let now = Instant::now();
        let mut body = vec![1];
        body.extend(entry("test", "1", 0, true));
        let mut negotiation = policy(true, true, now);
        assert!(matches!(
            negotiation.observe_custom_payload(&payload(CHANNEL, &body), now),
            Err(PolicyError::UnsupportedContent)
        ));
        assert!(matches!(
            negotiation.observe_custom_payload(&payload(CHANNEL, &[0]), now),
            Err(PolicyError::Closed)
        ));
    }

    #[test]
    fn malformed_lists_are_terminal_and_never_become_a_standard_profile() {
        let now = Instant::now();
        let mut duplicate = vec![2];
        duplicate.extend(entry("same", "1", 0, false));
        duplicate.extend(entry("same", "2", 0, false));
        let mut too_long = vec![1];
        too_long.extend(entry(&"a".repeat(65), "1", 0, false));
        let mut hash_truncated = vec![1];
        hash_truncated.extend(entry("a", "1", 0, true));
        hash_truncated.pop();
        let mut wrong_side = vec![1];
        wrong_side.extend(entry("a", "1", 3, false));
        let mut wrong_hash = vec![1];
        wrong_hash.extend(entry("a", "1", 0, false));
        *wrong_hash.last_mut().unwrap() = 2;
        for bytes in [
            vec![],
            vec![0, 1],
            vec![129, 1],
            vec![255, 255, 255, 255, 15],
            duplicate,
            too_long,
            hash_truncated,
            wrong_side,
            wrong_hash,
        ] {
            let mut negotiation = policy(true, true, now);
            assert!(
                negotiation
                    .observe_custom_payload(&payload(CHANNEL, &bytes), now)
                    .is_err()
            );
            assert!(matches!(negotiation.finish(now), Err(PolicyError::Closed)));
        }
    }

    #[test]
    fn payload_and_list_limits_are_enforced_before_unbounded_allocation() {
        let now = Instant::now();
        let mut negotiation = policy(true, true, now);
        assert!(
            negotiation
                .observe_custom_payload(&vec![0; MAX_PAYLOAD_BYTES + 1], now)
                .is_err()
        );
        let mut body = Vec::new();
        put_varint(128, &mut body);
        for index in 0..128 {
            body.extend(entry(&format!("item{index}"), "1", 0, false));
        }
        assert_eq!(parse_list(&body).unwrap(), 128);
        for end in 0..entry("a", "1", 0, true).len() {
            let mut body = vec![1];
            body.extend_from_slice(&entry("a", "1", 0, true)[..end]);
            assert!(parse_list(&body).is_err(), "truncated entry {end}");
        }
    }
}

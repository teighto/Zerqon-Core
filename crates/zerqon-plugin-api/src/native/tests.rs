use super::*;
use proptest::prelude::*;
use serde_json::json;
use std::io::{self, Read};
use uuid::Uuid;

fn position() -> NativePosition {
    NativePosition {
        x: 1.5,
        y: 64.0,
        z: -2.5,
        yaw: 90.0,
        pitch: -12.0,
    }
}

fn player(index: u32) -> NativePlayer {
    NativePlayer {
        uuid: Uuid::from_u128(u128::from(index)),
        session_id: index as i32,
        entity_id: index as i32,
        name: format!("Player{index}"),
        world_uuid: Uuid::from_u128(999_999),
        position: position(),
        operator: false,
        online: true,
    }
}

fn world(index: u32) -> NativeWorld {
    NativeWorld {
        uuid: Uuid::from_u128(u128::from(index)),
        name: format!("World{index}"),
        key: format!("minecraft:world{index}"),
        dimension: "minecraft:overworld".into(),
        environment: NativeEnvironment::Normal,
        min_height: -64,
        max_height: 320,
        spawn: position(),
    }
}

fn server() -> ServerSnapshot {
    ServerSnapshot {
        server_version: "0.0.1".into(),
        minecraft_version: "26.3".into(),
        protocol: 777,
        max_players: 100,
        online_players: 2,
        tick: 7,
    }
}

fn player_query(player: &NativePlayer) -> NativeQuery {
    NativeQuery::Player {
        uuid: player.uuid,
        session_id: Some(player.session_id),
    }
}

fn encode_success(value: NativeQueryValue) -> Vec<u8> {
    serde_json::to_vec(&NativeQueryResponse::Success { value }).unwrap()
}

#[test]
fn all_success_variants_roundtrip_with_their_query_contract() {
    let sample = player(1);
    for (query, value) in [
        (
            NativeQuery::ServerSnapshot,
            NativeQueryValue::ServerSnapshot(server()),
        ),
        (
            NativeQuery::OnlinePlayers,
            NativeQueryValue::OnlinePlayers {
                players: vec![sample.clone()],
            },
        ),
        (
            player_query(&sample),
            NativeQueryValue::Player {
                player: Some(sample.clone()),
            },
        ),
        (
            player_query(&sample),
            NativeQueryValue::Player { player: None },
        ),
        (
            NativeQuery::Worlds,
            NativeQueryValue::Worlds {
                worlds: vec![world(1)],
            },
        ),
    ] {
        let expected = NativeQueryResponse::Success { value };
        let decoded =
            NativeQueryResponse::decode_for(&query, &serde_json::to_vec(&expected).unwrap())
                .unwrap();
        assert!(decoded.valid_for(&query));
        assert_eq!(decoded, expected);
    }
}

#[test]
fn player_position_preserves_the_minimized_decimal_roundtrip_regression() {
    let mut sample = player(1);
    sample.position = NativePosition {
        x: -14_027_620.401_934_333,
        y: 0.0,
        z: 0.0,
        yaw: 90.0,
        pitch: -12.0,
    };
    let expected = NativeQueryResponse::Success {
        value: NativeQueryValue::Player {
            player: Some(sample.clone()),
        },
    };
    let bytes = serde_json::to_vec(&expected).unwrap();
    assert_eq!(
        NativeQueryResponse::decode_for(&player_query(&sample), &bytes).unwrap(),
        expected
    );
}

#[test]
fn server_snapshot_streaming_decoder_accepts_any_field_order_and_rejects_duplicates() {
    let fields = [
        r#""server_version":"0.0.1""#,
        r#""minecraft_version":"26.3""#,
        r#""protocol":777"#,
        r#""max_players":100"#,
        r#""online_players":2"#,
        r#""tick":7"#,
    ];
    for first in 0..fields.len() {
        let fields = fields
            .iter()
            .cycle()
            .skip(first)
            .take(fields.len())
            .copied()
            .collect::<Vec<_>>()
            .join(",");
        let bytes = format!(r#"{{"value":{{{fields}}},"ok":true}}"#);
        assert_eq!(
            NativeQueryResponse::decode_for(&NativeQuery::ServerSnapshot, bytes.as_bytes())
                .unwrap(),
            NativeQueryResponse::Success {
                value: NativeQueryValue::ServerSnapshot(server())
            }
        );
        let duplicate = format!(r#"{{"ok":true,"value":{{{fields},"tick":7}}}}"#);
        assert!(
            NativeQueryResponse::decode_for(&NativeQuery::ServerSnapshot, duplicate.as_bytes())
                .is_err()
        );
    }
}

fn extra_field(bytes: &[u8], name: &str) -> Vec<u8> {
    assert_eq!(bytes.last(), Some(&b'}'));
    let mut output = bytes[..bytes.len() - 1].to_vec();
    output
        .extend_from_slice(format!(",{}:true}}", serde_json::to_string(name).unwrap()).as_bytes());
    output
}

#[test]
fn query_wire_preserves_fieldless_shapes_and_optional_session_semantics() {
    for (query, expected) in [
        (
            NativeQuery::ServerSnapshot,
            json!({"query":"server_snapshot"}),
        ),
        (
            NativeQuery::OnlinePlayers,
            json!({"query":"online_players"}),
        ),
        (NativeQuery::Worlds, json!({"query":"worlds"})),
        (
            NativeQuery::Player {
                uuid: Uuid::from_u128(1),
                session_id: None,
            },
            json!({"query":"player","uuid":Uuid::from_u128(1)}),
        ),
    ] {
        assert_eq!(serde_json::to_value(&query).unwrap(), expected);
        assert_eq!(
            NativeQuery::decode(&serde_json::to_vec(&query).unwrap()).unwrap(),
            query
        );
    }
    let null_session = json!({"query":"player","uuid":Uuid::from_u128(1),"session_id":null});
    assert_eq!(
        NativeQuery::decode(&serde_json::to_vec(&null_session).unwrap()).unwrap(),
        NativeQuery::Player {
            uuid: Uuid::from_u128(1),
            session_id: None
        }
    );
}

#[test]
fn queries_reject_unknown_duplicate_missing_and_invalid_fields() {
    let uuid = Uuid::from_u128(1);
    let cases = [
        "{}".to_owned(),
        "[]".into(),
        "null".into(),
        r#"{"query":"unknown"}"#.into(),
        r#"{"query":"Worlds"}"#.into(),
        r#"{"query":null}"#.into(),
        r#"{"query":"worlds","uuid":null}"#.into(),
        r#"{"query":"server_snapshot","operator":true}"#.into(),
        r#"{"query":"online_players","query":"online_players"}"#.into(),
        r#"{"query":"player"}"#.into(),
        r#"{"query":"player","uuid":"not-a-uuid"}"#.into(),
        format!(r#"{{"query":"player","uuid":"{uuid}","uuid":"{uuid}"}}"#),
        format!(r#"{{"query":"player","uuid":"{uuid}","session_id":null,"session_id":1}}"#),
        format!(r#"{{"query":"player","uuid":"{uuid}","session_id":0}}"#),
        format!(r#"{{"query":"player","uuid":"{uuid}","session_id":-1}}"#),
        format!(r#"{{"query":"player","uuid":"{uuid}","session_id":2147483648}}"#),
        format!(r#"{{"query":"player","uuid":"{uuid}","session_id":"1"}}"#),
        format!(r#"{{"query":"player","uuid":"{}"}}"#, Uuid::nil()),
    ];
    for bytes in cases {
        assert!(NativeQuery::decode(bytes.as_bytes()).is_err(), "{bytes}");
    }
}

#[test]
fn required_nullable_player_distinguishes_null_from_missing_or_empty_object() {
    let query = player_query(&player(1));
    let valid = br#"{"ok":true,"value":{"player":null}}"#;
    let decoded = NativeQueryResponse::decode_for(&query, valid).unwrap();
    assert!(matches!(
        decoded,
        NativeQueryResponse::Success {
            value: NativeQueryValue::Player { player: None }
        }
    ));
    for bytes in [
        br#"{"ok":true,"value":{}}"#.as_slice(),
        br#"{"ok":true,"value":{"player":{}}}"#,
        br#"{"ok":true,"value":{"player":null,"player":null}}"#,
        br#"{"ok":true,"value":{"player":null,"worlds":[]}}"#,
        br#"{"ok":true,"value":{"players":[]}}"#,
    ] {
        assert!(
            NativeQueryResponse::decode_for(&query, bytes).is_err(),
            "{}",
            String::from_utf8_lossy(bytes)
        );
    }
    assert!(serde_json::from_slice::<NativeQueryValue>(b"{}").is_err());
}

#[test]
fn response_discriminant_and_envelopes_are_strict_boolean_protocol() {
    let query = NativeQuery::Worlds;
    let value = NativeQueryValue::Worlds { worlds: Vec::new() };
    assert_eq!(
        serde_json::to_value(NativeQueryResponse::Success { value }).unwrap(),
        json!({"ok":true,"value":{"worlds":[]}})
    );
    let error = NativeQueryError::new(NativeErrorCode::Unavailable, "not ready");
    assert_eq!(
        serde_json::to_value(NativeQueryResponse::Error { error }).unwrap(),
        json!({"ok":false,"error":{"code":"unavailable","message":"not ready"}})
    );
    for text in [
        "null",
        "[]",
        "{}",
        r#"{"value":{"worlds":[]}}"#,
        r#"{"ok":true}"#,
        r#"{"ok":false}"#,
        r#"{"ok":"true","value":{"worlds":[]}}"#,
        r#"{"ok":1,"value":{"worlds":[]}}"#,
        r#"{"ok":null,"value":{"worlds":[]}}"#,
        r#"{"ok":false,"value":{"worlds":[]}}"#,
        r#"{"ok":true,"error":{"code":"internal","message":"x"}}"#,
        r#"{"ok":true,"value":{"worlds":[]},"error":null}"#,
        r#"{"ok":false,"error":{"code":"internal","message":"x"},"value":null}"#,
        r#"{"ok":true,"ok":true,"value":{"worlds":[]}}"#,
        r#"{"ok":true,"value":{"worlds":[]},"value":{"worlds":[]}}"#,
        r#"{"ok":false,"error":{"code":"internal","message":"x"},"error":null}"#,
        r#"{"ok":false,"error":{"code":"INTERNAL","message":"x"}}"#,
        r#"{"ok":false,"error":{"code":"internal","code":"internal","message":"x"}}"#,
        r#"{"ok":false,"error":{"code":"internal","message":"x","message":"x"}}"#,
        r#"{"ok":true,"value":{"worlds":[]},"parent_id":1}"#,
    ] {
        assert!(
            NativeQueryResponse::decode_for(&query, text.as_bytes()).is_err(),
            "{text}"
        );
    }
}

#[test]
fn nested_payload_fields_cannot_be_duplicated_or_silently_ignored() {
    let sample = player(1);
    let query = player_query(&sample);
    let encoded = serde_json::to_string(&sample).unwrap();
    let duplicated = format!(r#"{{"uuid":"{}",{}"#, sample.uuid, &encoded[1..]);
    let duplicate_player = format!(r#"{{"ok":true,"value":{{"player":{duplicated}}}}}"#);
    assert!(NativeQueryResponse::decode_for(&query, duplicate_player.as_bytes()).is_err());
    let mut value = serde_json::to_value(&sample).unwrap();
    value["position"]["operator"] = json!(true);
    let unknown_position = json!({"ok":true,"value":{"player":value}});
    assert!(
        NativeQueryResponse::decode_for(&query, &serde_json::to_vec(&unknown_position).unwrap())
            .is_err()
    );
    for key in [
        "uuid",
        "session_id",
        "entity_id",
        "name",
        "world_uuid",
        "position",
        "operator",
        "online",
    ] {
        let mut value = serde_json::to_value(&sample).unwrap();
        value.as_object_mut().unwrap().remove(key);
        assert!(
            NativeQueryResponse::decode_for(
                &query,
                &serde_json::to_vec(&json!({"ok":true,"value":{"player":value}})).unwrap()
            )
            .is_err(),
            "missing {key}"
        );
    }
}

#[test]
fn decode_byte_caps_apply_before_json_and_allow_exact_cap_whitespace() {
    let mut query = br#"{"query":"worlds"}"#.to_vec();
    query.resize(MAX_NATIVE_CALL_BYTES, b' ');
    assert_eq!(NativeQuery::decode(&query).unwrap(), NativeQuery::Worlds);
    query.push(b' ');
    assert!(matches!(
        NativeQuery::decode(&query),
        Err(NativeDecodeError::TooLarge)
    ));

    let mut response = encode_success(NativeQueryValue::Worlds { worlds: Vec::new() });
    response.resize(MAX_NATIVE_RESULT_BYTES, b' ');
    assert!(NativeQueryResponse::decode_for(&NativeQuery::Worlds, &response).is_ok());
    response.push(b' ');
    assert!(matches!(
        NativeQueryResponse::decode_for(&NativeQuery::Worlds, &response),
        Err(NativeDecodeError::TooLarge)
    ));

    let mut context = br#"{"source":{"kind":"server"}}"#.to_vec();
    context.resize(MAX_NATIVE_CONTEXT_BYTES, b' ');
    assert!(NativeCallbackContext::decode(&context).is_ok());
    context.push(b' ');
    assert!(matches!(
        NativeCallbackContext::decode(&context),
        Err(NativeDecodeError::TooLarge)
    ));

    for bytes in [b"\xff".as_slice(), b"{}{}", b"{", b"[] trailing"] {
        assert!(NativeQuery::decode(bytes).is_err());
        assert!(NativeQueryResponse::decode_for(&NativeQuery::Worlds, bytes).is_err());
        assert!(NativeCallbackContext::decode(bytes).is_err());
    }
}

#[test]
fn callback_context_has_strict_source_fields_and_explicit_event_binding() {
    let uuid = Uuid::from_u128(1);
    let player_context = NativeCallbackContext {
        source: NativeSource::Player {
            uuid,
            session_id: 7,
        },
    };
    let server_context = NativeCallbackContext {
        source: NativeSource::Server,
    };
    let console_context = NativeCallbackContext {
        source: NativeSource::Console,
    };
    let player_command = crate::Event::Command {
        sender: "CONSOLE".into(),
        sender_kind: crate::SenderKind::Player,
        operator: false,
        command: "test".into(),
        args: Vec::new(),
    };
    let mut console_command = player_command.clone();
    if let crate::Event::Command { sender_kind, .. } = &mut console_command {
        *sender_kind = crate::SenderKind::Console;
    }
    assert!(player_context.matches_event(&player_command));
    assert!(!player_context.matches_event(&console_command));
    assert!(console_context.matches_event(&console_command));
    assert!(!console_context.matches_event(&player_command));
    assert!(!server_context.matches_event(&player_command));
    for event in [
        crate::Event::Enable {
            server_version: "0.0.1".into(),
        },
        crate::Event::Disable,
        crate::Event::Tick { tick: 1 },
    ] {
        assert!(server_context.matches_event(&event));
        assert!(!console_context.matches_event(&event));
        assert!(!player_context.matches_event(&event));
    }
    for event in [
        crate::Event::PlayerJoin {
            uuid: uuid.to_string(),
            name: "Player".into(),
        },
        crate::Event::PlayerQuit {
            uuid: uuid.to_string(),
            name: "Player".into(),
        },
    ] {
        assert!(player_context.matches_event(&event));
        assert!(!server_context.matches_event(&event));
    }
    assert!(!player_context.matches_event(&crate::Event::PlayerJoin {
        uuid: Uuid::from_u128(2).to_string(),
        name: "Player".into()
    }));
    for text in [
        "{}".to_owned(),
        r#"{"source":null}"#.into(),
        r#"{"source":{"kind":"server","uuid":null}}"#.into(),
        r#"{"source":{"kind":"console","operator":true}}"#.into(),
        r#"{"source":{"kind":"server","kind":"server"}}"#.into(),
        r#"{"source":{"kind":"server"},"source":{"kind":"server"}}"#.into(),
        r#"{"source":{"kind":"Server"}}"#.into(),
        format!(r#"{{"source":{{"kind":"player","uuid":"{uuid}"}}}}"#),
        format!(r#"{{"source":{{"kind":"player","uuid":"{uuid}","session_id":0}}}}"#),
        format!(
            r#"{{"source":{{"kind":"player","uuid":"{}","session_id":1}}}}"#,
            Uuid::nil()
        ),
    ] {
        assert!(
            NativeCallbackContext::decode(text.as_bytes()).is_err(),
            "{text}"
        );
    }
}

#[test]
fn nullable_success_still_requires_a_valid_query() {
    let bytes = br#"{"ok":true,"value":{"player":null}}"#;
    for query in [
        NativeQuery::Player {
            uuid: Uuid::nil(),
            session_id: None,
        },
        NativeQuery::Player {
            uuid: Uuid::from_u128(1),
            session_id: Some(0),
        },
    ] {
        assert!(NativeQueryResponse::decode_for(&query, bytes).is_err());
    }
}

#[test]
fn typed_service_errors_decode_even_when_query_arguments_are_invalid() {
    for query in [
        NativeQuery::Player {
            uuid: Uuid::nil(),
            session_id: None,
        },
        NativeQuery::Player {
            uuid: Uuid::from_u128(1),
            session_id: Some(0),
        },
        NativeQuery::Player {
            uuid: Uuid::from_u128(1),
            session_id: Some(-1),
        },
    ] {
        assert!(!query.valid());
        for code in [
            NativeErrorCode::InvalidArgument,
            NativeErrorCode::DeadlineExceeded,
        ] {
            let response = NativeQueryResponse::Error {
                error: NativeQueryError::new(code, "request could not be served"),
            };
            let bytes = serde_json::to_vec(&response).unwrap();
            assert_eq!(
                NativeQueryResponse::decode_for(&query, &bytes).unwrap(),
                response
            );
            assert!(response.valid_for(&query));
        }
    }
}

#[test]
fn list_identity_and_semantic_bounds_match_existing_p1_contract() {
    let first = player(1);
    for duplicated in [
        NativePlayer {
            uuid: first.uuid,
            ..player(2)
        },
        NativePlayer {
            session_id: first.session_id,
            ..player(2)
        },
        NativePlayer {
            entity_id: first.entity_id,
            ..player(2)
        },
        NativePlayer {
            online: false,
            ..player(2)
        },
    ] {
        let bytes = encode_success(NativeQueryValue::OnlinePlayers {
            players: vec![first.clone(), duplicated],
        });
        assert!(NativeQueryResponse::decode_for(&NativeQuery::OnlinePlayers, &bytes).is_err());
    }
    for duplicated in [
        NativeWorld {
            uuid: world(1).uuid,
            ..world(2)
        },
        NativeWorld {
            key: world(1).key,
            ..world(2)
        },
    ] {
        assert!(
            NativeQueryResponse::decode_for(
                &NativeQuery::Worlds,
                &encode_success(NativeQueryValue::Worlds {
                    worlds: vec![world(1), duplicated]
                })
            )
            .is_err()
        );
    }
    for value in [
        NativeQueryValue::OnlinePlayers {
            players: Vec::new(),
        },
        NativeQueryValue::Worlds { worlds: Vec::new() },
    ] {
        let query = if matches!(value, NativeQueryValue::OnlinePlayers { .. }) {
            NativeQuery::OnlinePlayers
        } else {
            NativeQuery::Worlds
        };
        assert!(NativeQueryResponse::decode_for(&query, &encode_success(value)).is_ok());
    }
}

struct CountingReader<'a> {
    bytes: &'a [u8],
    consumed: usize,
}
impl Read for CountingReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() || self.consumed == self.bytes.len() {
            return Ok(0);
        }
        output[0] = self.bytes[self.consumed];
        self.consumed += 1;
        Ok(1)
    }
}

#[test]
fn arrays_stop_at_the_limit_before_materializing_an_extra_element() {
    for (value, query) in [
        (
            NativeQueryValue::OnlinePlayers {
                players: (1..=MAX_QUERY_PLAYERS as u32).map(player).collect(),
            },
            NativeQuery::OnlinePlayers,
        ),
        (
            NativeQueryValue::Worlds {
                worlds: (1..=MAX_QUERY_WORLDS as u32).map(world).collect(),
            },
            NativeQuery::Worlds,
        ),
    ] {
        let mut bytes = serde_json::to_vec(&value).unwrap();
        let at_limit: NativeQueryValue = serde_json::from_slice(&bytes).unwrap();
        assert!(at_limit.valid_for(&query));
        assert!(bytes.ends_with(b"]}"));
        bytes.truncate(bytes.len() - 2);
        bytes.extend_from_slice(b", ");
        let cutoff = bytes.len();
        bytes.push(b'"');
        bytes.extend(std::iter::repeat_n(b'x', 65_536));
        bytes.extend_from_slice(b"\"]}");
        let mut reader = CountingReader {
            bytes: &bytes,
            consumed: 0,
        };
        assert!(serde_json::from_reader::<_, NativeQueryValue>(&mut reader).is_err());
        assert!(
            reader.consumed <= cutoff + 16,
            "over-limit element was parsed: consumed {} beyond {}",
            reader.consumed,
            cutoff
        );
    }
}

#[test]
fn utf8_numeric_and_height_boundaries_are_semantic_not_character_counts() {
    let mut snapshot = server();
    snapshot.server_version = "😀".repeat(32);
    assert!(snapshot.valid());
    snapshot.server_version.push('a');
    assert!(!snapshot.valid());
    for text in ["", "bad\nversion", "bad\u{7f}"] {
        snapshot.server_version = text.into();
        assert!(!snapshot.valid());
    }
    snapshot = server();
    snapshot.tick = i64::MAX as u64;
    assert!(snapshot.valid());
    snapshot.tick += 1;
    assert!(!snapshot.valid());
    snapshot = server();
    snapshot.max_players = 1_000_000;
    snapshot.online_players = 1_000_000;
    assert!(snapshot.valid());
    snapshot.max_players += 1;
    assert!(!snapshot.valid());
    for value in [
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::from_bits(30_000_000.0f64.to_bits() + 1),
    ] {
        let mut position = position();
        position.x = value;
        assert!(!position.valid());
    }
    let mut position = position();
    position.x = -30_000_000.0;
    position.y = 20_000_000.0;
    position.z = 30_000_000.0;
    assert!(position.valid());
    position.pitch = f32::INFINITY;
    assert!(!position.valid());
    let mut world = world(1);
    world.min_height = -20_000_000;
    world.max_height = 20_000_000;
    assert!(world.valid());
    world.max_height -= 1;
    assert!(!world.valid());
    world.max_height = world.min_height;
    assert!(!world.valid());
    let message = "😀".repeat(256);
    assert!(NativeQueryError::new(NativeErrorCode::Internal, &message).valid());
    assert!(!NativeQueryError::new(NativeErrorCode::Internal, format!("{message}a")).valid());
    for text in ["", "bad\0", "bad\n", "bad\u{85}"] {
        assert!(!NativeQueryError::new(NativeErrorCode::Internal, text).valid());
    }
}

fn text_strategy() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        prop_oneof![
            Just('a'),
            Just('Я'),
            Just('😀'),
            Just('"'),
            Just('\\'),
            Just(' ')
        ],
        1..=32,
    )
    .prop_map(|characters| characters.into_iter().collect())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn player_queries_and_context_roundtrip(uuid in 1u128..=u128::MAX, session in 1i32..=i32::MAX, exact in any::<bool>()) {
        let uuid = Uuid::from_u128(uuid);
        let query = NativeQuery::Player { uuid, session_id: exact.then_some(session) };
        prop_assert_eq!(NativeQuery::decode(&serde_json::to_vec(&query).unwrap()).unwrap(), query);
        let context = NativeCallbackContext { source: NativeSource::Player { uuid, session_id: session } };
        let decoded = NativeCallbackContext::decode(&serde_json::to_vec(&context).unwrap()).unwrap();
        prop_assert!(decoded.valid());
        prop_assert_eq!(serde_json::to_value(decoded).unwrap(), serde_json::to_value(context).unwrap());
    }

    #[test]
    fn player_response_roundtrip_and_wrong_session_rejection(uuid in 1u128..=u128::MAX, session in 1i32..=i32::MAX, x in -480_000_000i32..=480_000_000, y in -320_000_000i32..=320_000_000) {
        let mut value = player(1); value.uuid = Uuid::from_u128(uuid); value.session_id = session; value.entity_id = session;
        value.position.x = f64::from(x) / 16.0; value.position.y = f64::from(y) / 16.0;
        let query = player_query(&value);
        let bytes = encode_success(NativeQueryValue::Player { player: Some(value.clone()) });
        let result = NativeQueryResponse::decode_for(&query, &bytes).unwrap();
        let NativeQueryResponse::Success { value: NativeQueryValue::Player { player: Some(decoded) } } = result else { return Err(TestCaseError::fail("wrong response shape")); };
        prop_assert_eq!(decoded, value.clone());
        let wrong = NativeQuery::Player { uuid: value.uuid, session_id: Some(if session == i32::MAX { session - 1 } else { session + 1 }) };
        prop_assert!(NativeQueryResponse::decode_for(&wrong, &bytes).is_err());
        prop_assert!(NativeQueryResponse::decode_for(&NativeQuery::Worlds, &bytes).is_err());
    }

    #[test]
    fn arbitrary_finite_positions_roundtrip_without_precision_loss(x in -30_000_000.0f64..=30_000_000.0, y in -20_000_000.0f64..=20_000_000.0, z in -30_000_000.0f64..=30_000_000.0) {
        let mut value = player(1); value.position.x = x; value.position.y = y; value.position.z = z;
        let query = player_query(&value);
        let bytes = encode_success(NativeQueryValue::Player { player: Some(value.clone()) });
        let result = NativeQueryResponse::decode_for(&query, &bytes).unwrap();
        let NativeQueryResponse::Success { value: NativeQueryValue::Player { player: Some(decoded) } } = result else { return Err(TestCaseError::fail("wrong response shape")); };
        prop_assert_eq!(decoded.position, value.position);
    }

    #[test]
    fn service_error_roundtrip_preserves_code_and_unicode(message in text_strategy(), selected in 0usize..9) {
        let codes = [NativeErrorCode::InvalidArgument, NativeErrorCode::NotFound, NativeErrorCode::StaleSession, NativeErrorCode::Unavailable, NativeErrorCode::Unsupported, NativeErrorCode::Capacity, NativeErrorCode::Cancelled, NativeErrorCode::DeadlineExceeded, NativeErrorCode::Internal];
        let error = NativeQueryError::new(codes[selected], message);
        let bytes = serde_json::to_vec(&NativeQueryResponse::Error { error: error.clone() }).unwrap();
        let decoded = NativeQueryResponse::decode_for(&NativeQuery::ServerSnapshot, &bytes).unwrap();
        let NativeQueryResponse::Error { error: decoded } = decoded else { return Err(TestCaseError::fail("expected typed service error")); };
        prop_assert_eq!(decoded, error);
    }

    #[test]
    fn extra_fields_are_never_ignored(suffix in "[a-z0-9_]{1,12}") {
        let key = format!("extra_{suffix}");
        prop_assert!(NativeQuery::decode(&extra_field(br#"{"query":"worlds"}"#, &key)).is_err(), "query accepted an extra field");
        prop_assert!(NativeCallbackContext::decode(&extra_field(br#"{"source":{"kind":"server"}}"#, &key)).is_err(), "context accepted an extra field");
        prop_assert!(serde_json::from_slice::<NativeQueryValue>(&extra_field(br#"{"player":null}"#, &key)).is_err(), "value accepted an extra field");
        prop_assert!(NativeQueryResponse::decode_for(&NativeQuery::Worlds, &extra_field(br#"{"ok":true,"value":{"worlds":[]}}"#, &key)).is_err(), "response accepted an extra field");
    }
}

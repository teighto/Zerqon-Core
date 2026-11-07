include!("../src/plugin_services.rs");

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn position() -> NativePosition {
        NativePosition {
            x: -16.5,
            y: 64.0,
            z: 0.5,
            yaw: 12.0,
            pitch: -30.0,
        }
    }

    fn player() -> NativePlayer {
        NativePlayer {
            uuid: Uuid::from_u128(1),
            session_id: 7,
            entity_id: 7,
            name: "CONSOLE".into(),
            world_uuid: Uuid::from_u128(2),
            position: position(),
            operator: false,
            online: true,
        }
    }

    fn world() -> NativeWorld {
        NativeWorld {
            uuid: Uuid::from_u128(2),
            name: "world".into(),
            key: "minecraft:overworld".into(),
            dimension: "minecraft:overworld".into(),
            environment: NativeEnvironment::Normal,
            min_height: -64,
            max_height: 320,
            spawn: position(),
        }
    }

    #[test]
    fn read_model_queries_parse_without_changing_existing_snapshot_request() {
        for request in [
            r#"{"query":"server_snapshot"}"#,
            r#"{"query":"online_players"}"#,
            r#"{"query":"worlds"}"#,
            r#"{"query":"player","uuid":"00000000-0000-0000-0000-000000000001"}"#,
            r#"{"query":"player","uuid":"00000000-0000-0000-0000-000000000001","session_id":5}"#,
        ] {
            let parsed = serde_json::from_str::<NativeQuery>(request);
            assert!(parsed.is_ok(), "{request}: {parsed:?}");
        }
        for query in ["online_players", "server_snapshot", "worlds"] {
            assert!(
                serde_json::from_str::<NativeQuery>(&format!(
                    r#"{{"query":"{query}","include_offline":true}}"#
                ))
                .is_err()
            );
        }
    }

    #[test]
    fn read_values_reject_wrong_result_kind_identity_duplicates_and_capacity() {
        let snapshot = player();
        let query = NativeQuery::Player {
            uuid: snapshot.uuid,
            session_id: Some(snapshot.session_id),
        };
        assert!(
            NativeQueryValue::Player {
                player: Some(snapshot.clone())
            }
            .valid_for(&query)
        );
        assert!(NativeQueryValue::Player { player: None }.valid_for(&query));
        assert!(
            !NativeQueryValue::Player {
                player: Some(NativePlayer {
                    session_id: 8,
                    ..snapshot.clone()
                })
            }
            .valid_for(&query)
        );
        assert!(
            !NativeQueryValue::OnlinePlayers {
                players: vec![snapshot.clone()]
            }
            .valid_for(&query)
        );
        assert!(
            !NativeQueryValue::OnlinePlayers {
                players: vec![snapshot.clone(), snapshot.clone()]
            }
            .valid_for(&NativeQuery::OnlinePlayers)
        );
        assert!(
            !NativeQueryValue::OnlinePlayers {
                players: vec![
                    snapshot.clone(),
                    NativePlayer {
                        uuid: Uuid::from_u128(3),
                        session_id: 8,
                        ..snapshot.clone()
                    }
                ]
            }
            .valid_for(&NativeQuery::OnlinePlayers)
        );
        assert!(
            !NativeQueryValue::OnlinePlayers {
                players: vec![snapshot.clone(); MAX_QUERY_PLAYERS + 1]
            }
            .valid_for(&NativeQuery::OnlinePlayers)
        );
        assert!(
            !NativeQueryValue::OnlinePlayers {
                players: vec![NativePlayer {
                    online: false,
                    ..snapshot
                }]
            }
            .valid_for(&NativeQuery::OnlinePlayers)
        );
        assert!(
            NativeQueryValue::Worlds {
                worlds: vec![world()]
            }
            .valid_for(&NativeQuery::Worlds)
        );
        assert!(
            !NativeQueryValue::Worlds {
                worlds: vec![world(), world()]
            }
            .valid_for(&NativeQuery::Worlds)
        );
    }

    #[test]
    fn player_queries_reject_nil_uuid_and_nonpositive_sessions() {
        assert!(
            !NativeQuery::Player {
                uuid: Uuid::nil(),
                session_id: None
            }
            .valid()
        );
        for session_id in [0, -1, i32::MIN] {
            assert!(
                !NativeQuery::Player {
                    uuid: Uuid::from_u128(1),
                    session_id: Some(session_id)
                }
                .valid()
            );
        }
    }

    #[test]
    fn positions_and_worlds_require_finite_bounded_real_data() {
        for value in [f64::NAN, f64::INFINITY, -30_000_000.1, 30_000_000.1] {
            assert!(
                !NativePosition {
                    x: value,
                    ..position()
                }
                .valid()
            );
        }
        assert!(
            !NativePosition {
                yaw: f32::NAN,
                ..position()
            }
            .valid()
        );
        assert!(
            !NativeWorld {
                min_height: 320,
                ..world()
            }
            .valid()
        );
        assert!(
            !NativeWorld {
                max_height: 321,
                ..world()
            }
            .valid()
        );
        assert!(
            !NativeWorld {
                key: "minecraft:overworld:other".into(),
                ..world()
            }
            .valid()
        );
        assert!(
            !NativeWorld {
                name: "world\u{0}".into(),
                ..world()
            }
            .valid()
        );
    }

    #[test]
    fn event_context_preserves_player_identity_even_for_console_named_player() {
        use zerqon_plugin_api::{Event, SenderKind};
        let context = NativeEventContext { player: player() };
        let command = |sender_kind, operator| Event::Command {
            sender: "CONSOLE".into(),
            sender_kind,
            operator,
            command: "probe".into(),
            args: Vec::new(),
        };
        assert!(context.matches_event(&command(SenderKind::Player, false)));
        assert!(!context.matches_event(&command(SenderKind::Console, false)));
        assert!(!context.matches_event(&command(SenderKind::Player, true)));
        let quit = Event::PlayerQuit {
            uuid: context.player.uuid.to_string(),
            name: context.player.name.clone(),
        };
        assert!(context.matches_event(&quit));
        assert!(
            !NativeEventContext {
                player: NativePlayer {
                    online: false,
                    ..player()
                }
            }
            .matches_event(&quit)
        );
        assert!(!context.matches_event(&Event::PlayerJoin {
            uuid: Uuid::from_u128(3).to_string(),
            name: "CONSOLE".into()
        }));
        assert!(!context.matches_event(&Event::Tick { tick: 1 }));
    }
}

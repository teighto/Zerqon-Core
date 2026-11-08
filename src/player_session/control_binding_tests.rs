use super::*;
use crate::{
    config::{Properties, Settings},
    lifecycle::Lifecycle,
    player_control::{ControlError, ControlOutcome, MESSAGE_RESERVATION_BYTES, channel},
    players::Identity,
};
use std::{future::Future, path::Path, pin::Pin, task::Poll, time::Duration};
use tokio::{sync::mpsc, time::Instant};
use zerqon_plugin_api::{
    Action, Event, SenderKind,
    native::{NativeCallbackContext, NativeSource},
};

struct Fixture {
    state: Arc<State>,
    _directory: tempfile::TempDir,
    slots: usize,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        crate::config::initialize(directory.path()).unwrap();
        let lifecycle = Lifecycle::new();
        let state = State::new_with_lifecycle(
            directory.path(),
            Properties::parse("online-mode=false\nmax-players=2\nview-distance=1\n").unwrap(),
            Settings {
                data_directory: Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join(".cache/vanilla/26.3")
                    .to_string_lossy()
                    .into_owned(),
                java_plugins: false,
                spawn_mobs: false,
                update_check: false,
                ..Settings::default()
            },
            false,
            None,
            &lifecycle,
            &lifecycle.token(),
        )
        .await
        .unwrap();
        let observer = wat::parse_str(
            r#"(module
            (import "zerqon" "event_len" (func $len (result i32)))
            (import "zerqon" "event_read" (func $read (param i32 i32) (result i32)))
            (import "zerqon" "emit" (func $emit (param i32 i32) (result i32)))
            (memory (export "memory") 1)
            (global $quits (mut i32) (i32.const 0))
            (data (i32.const 1024) "{\"action\":\"reply\",\"message\":\"0\"}")
            (data (i32.const 1088) "{\"action\":\"reply\",\"message\":\"1\"}")
            (data (i32.const 1152) "{\"action\":\"reply\",\"message\":\"2\"}")
            (func (export "zerqon_abi_version") (result i32) i32.const 1)
            (func (export "zerqon_on_event") (result i32)
              i32.const 0 call $len call $read drop
              i32.const 17 i32.load8_u i32.const 113 i32.eq
              if global.get $quits i32.const 1 i32.add global.set $quits end
              i32.const 10 i32.load8_u i32.const 99 i32.eq
              if global.get $quits i32.eqz
                if (result i32) i32.const 1024
                else global.get $quits i32.const 1 i32.eq
                  if (result i32) i32.const 1088 else i32.const 1152 end
                end i32.const 32 call $emit drop
              end i32.const 0))"#,
        )
        .unwrap();
        drop(
            state
                .plugins
                .load(
                    "control-quit-counter",
                    &observer,
                    &lifecycle.token(),
                    deadline(),
                )
                .await
                .unwrap(),
        );
        let slots = state.slots.available_permits();
        Self {
            state,
            _directory: directory,
            slots,
        }
    }

    async fn session(&self) -> ConnectionSession {
        let uuid = Uuid::from_u128(84);
        let (outbound, _receiver) = mpsc::channel(32);
        let (disconnect, _closed) = watch::channel(false);
        let player = self
            .state
            .players
            .reserve(
                Identity {
                    uuid,
                    name: "ControlBinding".into(),
                    properties: Vec::new(),
                },
                outbound,
                disconnect,
            )
            .unwrap();
        let inventory = self
            .state
            .inventories
            .as_ref()
            .unwrap()
            .open(uuid, player.id())
            .await
            .unwrap();
        player.activate().unwrap();
        let slot = Arc::clone(&self.state.slots).acquire_owned().await.unwrap();
        let session = ConnectionSession::new(
            Arc::clone(&self.state),
            uuid,
            "ControlBinding".into(),
            player,
            inventory,
            slot,
        )
        .unwrap();
        session.join().await.unwrap();
        session
    }

    async fn quits(&self) -> String {
        let mut results = self
            .state
            .plugins
            .dispatch(
                &Event::Command {
                    sender: "CONSOLE".into(),
                    sender_kind: SenderKind::Console,
                    operator: true,
                    command: "count".into(),
                    args: Vec::new(),
                },
                &NativeCallbackContext {
                    source: NativeSource::Console,
                },
                Some("control-quit-counter"),
                &self.state.lifecycle.token(),
                deadline(),
            )
            .await;
        let output = results.pop().unwrap().result.unwrap();
        match output.actions() {
            [Action::Reply { message }] => message.clone(),
            actions => panic!("Quit observer returned unexpected actions: {actions:?}"),
        }
    }

    async fn finish(&self) {
        completed(self.state.sessions.drain()).await;
        assert_eq!(self.state.players.count(), 0);
        assert_eq!(self.state.slots.available_permits(), self.slots);
        assert_eq!(
            self.state.player_control_budget.snapshot().reserved_bytes,
            0
        );
        self.state.lifecycle.stop();
        let disabled = self.state.plugins.shutdown(deadline()).await;
        assert!(disabled.iter().all(|result| result.result.is_ok()));
        drop(disabled);
        self.state.profiler.shutdown().await.unwrap();
        self.state.lifecycle.drain(deadline()).await.unwrap();
    }
}

fn key(session: &ConnectionSession) -> SessionKey {
    SessionKey {
        uuid: session.inner.uuid,
        session_id: session.inner.id,
    }
}

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(5)
}

async fn completed<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("control/session fixture exceeded its completion deadline")
}

async fn pending<F: Future>(mut future: Pin<&mut F>) {
    std::future::poll_fn(|context| {
        assert!(future.as_mut().poll(context).is_pending());
        Poll::Ready(())
    })
    .await;
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft 26.3 data"]
async fn registry_control_requires_an_exact_bound_session_and_closes_with_it() {
    let fixture = Fixture::new().await;
    let session = fixture.session().await;
    let target = key(&session);
    let wrong_uuid = SessionKey {
        uuid: Uuid::from_u128(85),
        ..target
    };
    let wrong_generation = SessionKey {
        session_id: target.session_id + 1,
        ..target
    };
    assert!(fixture.state.sessions.control(target).is_none());
    let (wrong_sender, wrong_receiver, wrong_transport) = channel(
        wrong_uuid,
        fixture.state.player_control_budget.clone(),
        fixture.state.lifecycle.clone(),
        1,
    )
    .unwrap();
    assert!(session.bind_control(wrong_sender).is_err());
    drop((wrong_receiver, wrong_transport));
    let (sender, receiver, transport) = channel(
        target,
        fixture.state.player_control_budget.clone(),
        fixture.state.lifecycle.clone(),
        1,
    )
    .unwrap();
    let binding = session.bind_control(sender.clone()).unwrap();
    assert_eq!(
        fixture.state.sessions.control(target).unwrap().target(),
        target
    );
    assert!(fixture.state.sessions.control(wrong_uuid).is_none());
    assert!(fixture.state.sessions.control(wrong_generation).is_none());
    assert!(session.bind_control(sender).is_err());
    completed(session.close()).await.unwrap();
    assert!(fixture.state.sessions.control(target).is_none());
    assert_eq!(fixture.quits().await, "1");
    drop((binding, receiver, transport, session));
    fixture.finish().await;
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft 26.3 data"]
async fn retained_old_control_and_binding_cannot_reach_or_unbind_a_reconnected_player() {
    let fixture = Fixture::new().await;
    let old_session = fixture.session().await;
    let old_key = key(&old_session);
    let (old_sender, old_receiver, old_transport) = channel(
        old_key,
        fixture.state.player_control_budget.clone(),
        fixture.state.lifecycle.clone(),
        1,
    )
    .unwrap();
    let old_binding = old_session.bind_control(old_sender.clone()).unwrap();
    drop((old_receiver, old_transport));
    completed(old_session.close()).await.unwrap();
    drop(old_session);
    assert_eq!(
        completed(fixture.state.sessions.wait_for_login(old_key.uuid)).await,
        LoginAdmission::Available
    );
    let new_session = fixture.session().await;
    let new_key = key(&new_session);
    assert_eq!(old_key.uuid, new_key.uuid);
    assert_ne!(old_key.session_id, new_key.session_id);
    let (new_sender, mut new_receiver, new_transport) = channel(
        new_key,
        fixture.state.player_control_budget.clone(),
        fixture.state.lifecycle.clone(),
        1,
    )
    .unwrap();
    let new_binding = new_session.bind_control(new_sender).unwrap();
    drop(old_binding);
    assert!(fixture.state.sessions.control(old_key).is_none());
    assert!(matches!(
        old_sender.try_system_message("old session", deadline(), &fixture.state.lifecycle.token()),
        Err(ControlError::Closed)
    ));
    let current = fixture.state.sessions.control(new_key).unwrap();
    let receipt = current
        .try_system_message(
            "replacement session",
            deadline(),
            &fixture.state.lifecycle.token(),
        )
        .unwrap();
    let writing = completed(new_receiver.recv())
        .await
        .unwrap()
        .begin_write(new_key)
        .unwrap();
    assert_eq!(
        writing.packet(),
        crate::packets::system_chat("replacement session")
    );
    writing.finish_written();
    assert!(matches!(
        completed(receipt.wait()).await,
        ControlOutcome::Written { target, .. } if target == new_key
    ));
    let mut next = Box::pin(new_receiver.recv());
    pending(next.as_mut()).await;
    drop(next);
    drop((new_receiver, new_transport));
    completed(new_session.close()).await.unwrap();
    assert_eq!(fixture.quits().await, "2");
    drop((new_binding, new_session));
    fixture.finish().await;
}

#[tokio::test]
#[ignore = "requires prepared official Minecraft 26.3 data"]
async fn closing_session_waits_for_started_control_transport_before_quit_and_resource_release() {
    let fixture = Fixture::new().await;
    let session = fixture.session().await;
    let target = key(&session);
    let (sender, mut receiver, transport) = channel(
        target,
        fixture.state.player_control_budget.clone(),
        fixture.state.lifecycle.clone(),
        1,
    )
    .unwrap();
    let binding = session.bind_control(sender.clone()).unwrap();
    let receipt = sender
        .try_system_message("in progress", deadline(), &fixture.state.lifecycle.token())
        .unwrap();
    let writing = completed(receiver.recv())
        .await
        .unwrap()
        .begin_write(target)
        .unwrap();
    writing.finish_closed();
    drop(receiver);
    let mut receipt = Box::pin(receipt.wait());
    pending(receipt.as_mut()).await;
    let mut close = Box::pin(session.close());
    let closed_early = tokio::time::timeout(Duration::from_millis(80), close.as_mut())
        .await
        .is_ok();
    let lookup_during_close = fixture.state.sessions.control(target).is_some();
    let quits_before_transport_close = fixture.quits().await;
    let bytes_before_transport_close = fixture
        .state
        .player_control_budget
        .snapshot()
        .reserved_bytes;
    let player_retained = fixture
        .state
        .players
        .query_snapshot(target.uuid)
        .unwrap()
        .is_some();
    let slots_before_transport_close = fixture.state.slots.available_permits();
    drop(transport);
    if !closed_early {
        completed(close.as_mut()).await.unwrap();
    }
    drop(close);
    let outcome = completed(receipt).await;
    let quits_after_transport_close = fixture.quits().await;
    drop((binding, session));
    fixture.finish().await;
    assert!(
        !closed_early,
        "session close overtook its started control write"
    );
    assert!(
        !lookup_during_close,
        "closing session remained discoverable"
    );
    assert_eq!(quits_before_transport_close, "0");
    assert_eq!(bytes_before_transport_close, MESSAGE_RESERVATION_BYTES);
    assert!(player_retained);
    assert_eq!(slots_before_transport_close, fixture.slots - 1);
    assert!(matches!(
        outcome,
        ControlOutcome::ConnectionClosed {
            target: closed_target,
            may_have_written: true,
            ..
        } if closed_target == target
    ));
    assert_eq!(quits_after_transport_close, "1");
}

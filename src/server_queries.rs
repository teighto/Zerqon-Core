use crate::{
    players::Players,
    plugin_services::{
        NativeCallContext, NativeErrorCode, NativeQuery, NativeQueryError, NativeQueryFuture,
        NativeQueryService, ServerSnapshot,
    },
};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::{sync::watch, time::Instant};

pub struct ServerQueries {
    pub players: Arc<Players>,
    pub tick: Arc<AtomicU64>,
    pub max_players: u32,
    pub shutdown: watch::Receiver<bool>,
}

impl NativeQueryService for ServerQueries {
    fn query(&self, context: NativeCallContext, query: NativeQuery) -> NativeQueryFuture<'_> {
        Box::pin(async move {
            if context.deadline <= Instant::now() {
                return Err(NativeQueryError::new(
                    NativeErrorCode::DeadlineExceeded,
                    "Native query deadline expired",
                ));
            }
            if *self.shutdown.borrow() {
                return Err(NativeQueryError::new(
                    NativeErrorCode::Unavailable,
                    "Server is stopping",
                ));
            }
            if context.plugin.is_empty() || context.parent_id == 0 || context.call_id == 0 {
                return Err(NativeQueryError::new(
                    NativeErrorCode::InvalidArgument,
                    "Invalid native call identity",
                ));
            }
            match query {
                NativeQuery::ServerSnapshot => {
                    let online = self.players.try_count().map_err(|_| {
                        NativeQueryError::new(
                            NativeErrorCode::Unavailable,
                            "Player registry is unavailable",
                        )
                    })?;
                    Ok(ServerSnapshot {
                        server_version: env!("CARGO_PKG_VERSION").into(),
                        minecraft_version: "26.3".into(),
                        protocol: 777,
                        max_players: self.max_players,
                        online_players: u32::try_from(online).map_err(|_| {
                            NativeQueryError::new(
                                NativeErrorCode::Capacity,
                                "Player count exceeds the query limit",
                            )
                        })?,
                        tick: self.tick.load(Ordering::Acquire),
                    })
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::players::{EntityIds, Identity};
    use tokio::sync::mpsc;
    use uuid::Uuid;

    fn context() -> NativeCallContext {
        NativeCallContext {
            plugin: "teighto-query-test".into(),
            parent_id: 1,
            call_id: 1,
            deadline: Instant::now() + std::time::Duration::from_secs(2),
        }
    }

    #[tokio::test]
    async fn queries_follow_actual_player_activation_disconnect_ticks_and_shutdown() {
        let players = Players::new(4, 64.0, 159, Arc::new(EntityIds::default()));
        let (shutdown, receiver) = watch::channel(false);
        let service = ServerQueries {
            players: Arc::clone(&players),
            tick: Arc::new(AtomicU64::new(0)),
            max_players: 4,
            shutdown: receiver,
        };
        let (sender, _outbound) = mpsc::channel(8);
        let (disconnect, _disconnected) = watch::channel(false);
        let player = players
            .reserve(
                Identity {
                    uuid: Uuid::from_u128(7),
                    name: "teighto".into(),
                    properties: Vec::new(),
                },
                sender,
                disconnect,
            )
            .unwrap();
        assert_eq!(
            service
                .query(context(), NativeQuery::ServerSnapshot)
                .await
                .unwrap()
                .online_players,
            0
        );
        player.activate().unwrap();
        service.tick.store(37, Ordering::Release);
        let snapshot = service
            .query(context(), NativeQuery::ServerSnapshot)
            .await
            .unwrap();
        assert_eq!(
            (snapshot.online_players, snapshot.max_players, snapshot.tick),
            (1, 4, 37)
        );
        drop(player);
        assert_eq!(
            service
                .query(context(), NativeQuery::ServerSnapshot)
                .await
                .unwrap()
                .online_players,
            0
        );
        shutdown.send_replace(true);
        assert_eq!(
            service
                .query(context(), NativeQuery::ServerSnapshot)
                .await
                .unwrap_err()
                .code,
            NativeErrorCode::Unavailable
        );
    }

    #[tokio::test]
    async fn expired_queries_are_rejected_before_serving_a_snapshot() {
        let (_shutdown, receiver) = watch::channel(false);
        let service = ServerQueries {
            players: Players::new(4, 64.0, 159, Arc::new(EntityIds::default())),
            tick: Arc::new(AtomicU64::new(0)),
            max_players: 4,
            shutdown: receiver,
        };
        let mut context = context();
        context.deadline = Instant::now();
        assert_eq!(
            service
                .query(context, NativeQuery::ServerSnapshot)
                .await
                .unwrap_err()
                .code,
            NativeErrorCode::DeadlineExceeded
        );
    }
}

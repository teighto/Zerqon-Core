use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::watch;
use uuid::Uuid;

pub const PLUGIN_NAME: &str = "ZerqonSpeed";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlightSpeed(u8);

impl Default for FlightSpeed {
    fn default() -> Self {
        Self(1)
    }
}

impl FlightSpeed {
    pub fn parse(value: &str) -> Result<Self, FlightSpeedError> {
        let level = value.parse::<u8>().map_err(|_| FlightSpeedError::Range)?;
        if !(1..=10).contains(&level) {
            return Err(FlightSpeedError::Range);
        }
        Ok(Self(level))
    }

    pub fn level(self) -> u8 {
        self.0
    }
    pub fn wire(self) -> f32 {
        0.05 * f32::from(self.0)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FlightSpeedError {
    #[error("Speed must be an integer in 1..10")]
    Range,
    #[error("Flight speed target is no longer connected")]
    StaleTarget,
    #[error("Flight speed session capacity reached")]
    Capacity,
    #[error("Flight speed service is unavailable")]
    Unavailable,
}

pub struct FlightSpeedPlugin {
    sessions: Mutex<HashMap<(Uuid, i32), watch::Sender<FlightSpeed>>>,
    capacity: usize,
}

impl FlightSpeedPlugin {
    pub fn new(capacity: usize) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            capacity,
        }
    }

    pub fn register(
        self: &Arc<Self>,
        uuid: Uuid,
        id: i32,
    ) -> Result<FlightSpeedSession, FlightSpeedError> {
        if uuid.is_nil() || id <= 0 {
            return Err(FlightSpeedError::StaleTarget);
        }
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| FlightSpeedError::Unavailable)?;
        if sessions.contains_key(&(uuid, id)) {
            return Err(FlightSpeedError::StaleTarget);
        }
        if sessions.len() >= self.capacity {
            return Err(FlightSpeedError::Capacity);
        }
        let (sender, receiver) = watch::channel(FlightSpeed::default());
        sessions.insert((uuid, id), sender);
        Ok(FlightSpeedSession {
            plugin: Arc::downgrade(self),
            key: (uuid, id),
            receiver,
        })
    }

    pub fn set(&self, uuid: Uuid, id: i32, speed: FlightSpeed) -> Result<(), FlightSpeedError> {
        self.sessions
            .lock()
            .map_err(|_| FlightSpeedError::Unavailable)?
            .get(&(uuid, id))
            .ok_or(FlightSpeedError::StaleTarget)?
            .send(speed)
            .map_err(|_| FlightSpeedError::StaleTarget)
    }
}

pub struct FlightSpeedSession {
    plugin: Weak<FlightSpeedPlugin>,
    key: (Uuid, i32),
    receiver: watch::Receiver<FlightSpeed>,
}

impl FlightSpeedSession {
    pub fn current(&mut self) -> FlightSpeed {
        *self.receiver.borrow_and_update()
    }

    pub async fn changed(&mut self) -> Result<FlightSpeed, FlightSpeedError> {
        self.receiver
            .changed()
            .await
            .map_err(|_| FlightSpeedError::Unavailable)?;
        Ok(self.current())
    }
}

impl Drop for FlightSpeedSession {
    fn drop(&mut self) {
        if let Some(plugin) = self.plugin.upgrade() {
            plugin
                .sessions
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove(&self.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_are_bounded_and_old_sessions_cannot_change_a_reconnected_player() {
        assert_eq!(FlightSpeed::parse("1").unwrap().wire(), 0.05);
        assert_eq!(FlightSpeed::parse("10").unwrap().wire(), 0.5);
        for invalid in ["0", "11", "-1", "1.5", "NaN", ""] {
            assert!(FlightSpeed::parse(invalid).is_err());
        }
        let plugin = Arc::new(FlightSpeedPlugin::new(1));
        let uuid = Uuid::from_u128(42);
        let mut first = plugin.register(uuid, 1).unwrap();
        assert!(plugin.register(uuid, 2).is_err());
        plugin
            .set(uuid, 1, FlightSpeed::parse("4").unwrap())
            .unwrap();
        assert_eq!(first.current().wire(), 0.2);
        drop(first);
        let mut second = plugin.register(uuid, 2).unwrap();
        assert!(
            plugin
                .set(uuid, 1, FlightSpeed::parse("10").unwrap())
                .is_err()
        );
        assert_eq!(second.current(), FlightSpeed::default());
    }
}

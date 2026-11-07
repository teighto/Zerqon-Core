use crate::lifecycle::Lifecycle;
use std::{io, path::PathBuf, time::Duration};
use tokio::task::JoinHandle;

pub struct ShutdownSignals(Option<JoinHandle<()>>);

impl ShutdownSignals {
    pub fn install(lifecycle: Lifecycle, mut marker: Option<PathBuf>) -> io::Result<Self> {
        #[cfg(unix)]
        let mut primary =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        #[cfg(unix)]
        let mut secondary =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        #[cfg(windows)]
        let mut primary = tokio::signal::windows::ctrl_c()?;
        #[cfg(windows)]
        let mut secondary = tokio::signal::windows::ctrl_break()?;
        if marker.as_ref().is_some_and(|path| path.exists()) {
            lifecycle.stop();
            marker = None;
        }
        Ok(Self(Some(tokio::spawn(async move {
            let mut timer = tokio::time::interval(Duration::from_millis(50));
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    received = primary.recv() => {
                        if received.is_none() { return; }
                        lifecycle.stop();
                    },
                    received = secondary.recv() => {
                        if received.is_none() { return; }
                        lifecycle.stop();
                    },
                    _ = timer.tick(), if marker.is_some() => {
                        if marker.as_ref().is_some_and(|path| path.exists()) {
                            lifecycle.stop();
                            marker = None;
                        }
                    },
                }
            }
        }))))
    }

    pub async fn close(mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for ShutdownSignals {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}

use crate::store::{self, Db};
use anyhow::Result;
use backoff::{ExponentialBackoffBuilder, backoff::Backoff};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
pub(crate) struct Pending {
    pub attempts: u32,
    pub next_at: i64,
}
impl Pending {
    pub fn after_failure(self, now: i64, minimum: Duration) -> Option<Self> {
        const MAX_RETRIES: u32 = 5;
        if self.attempts >= MAX_RETRIES {
            return None;
        }
        let mut backoff = ExponentialBackoffBuilder::new()
            .with_initial_interval(Duration::from_secs(30))
            .with_multiplier(2.0)
            .with_randomization_factor(0.25)
            .with_max_interval(Duration::from_secs(480))
            .with_max_elapsed_time(None)
            .build();
        let delay = (0..=self.attempts)
            .filter_map(|_| backoff.next_backoff())
            .last()?
            .max(minimum);
        Some(Self {
            attempts: self.attempts + 1,
            next_at: now.saturating_add(
                i64::try_from(
                    delay
                        .as_secs()
                        .saturating_add(u64::from(delay.subsec_nanos() > 0)),
                )
                .unwrap_or(i64::MAX),
            ),
        })
    }
}

#[derive(Debug)]
pub(crate) struct Transient {
    pub message: String,
    pub after: Duration,
}

impl std::fmt::Display for Transient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Transient {}

pub(crate) fn minimum(error: &anyhow::Error) -> Option<Duration> {
    if let Some(error) = error.downcast_ref::<Transient>() {
        return Some(error.after);
    }
    let error = error.downcast_ref::<reqwest::Error>()?;
    (error.is_timeout()
        || error.is_connect()
        || error.is_body()
        || error
            .status()
            .is_some_and(|status| matches!(status.as_u16(), 408 | 429 | 500..=599)))
    .then_some(Duration::ZERO)
}

pub(crate) fn enabled(db: &mut Db, task: &str) -> Result<bool> {
    Ok(store::meta(db, &format!("auto_retry_{task}"))?.as_deref() != Some("0"))
}

pub(crate) fn pending(db: &mut Db, task: &str) -> Result<Option<Pending>> {
    store::meta(db, &format!("retry_pending_{task}"))?
        .map(|value| Ok(serde_json::from_str(&value)?))
        .transpose()
}

pub(crate) fn store_pending(db: &mut Db, task: &str, value: Option<Pending>) -> Result<()> {
    let key = format!("retry_pending_{task}");
    if let Some(value) = value {
        store::set_meta(db, &key, &serde_json::to_string(&value)?)
    } else {
        store::delete_meta(db, &key)
    }
}

pub(crate) fn configure(db: &mut Db, task: &str, enabled: bool) -> Result<()> {
    store::set_meta(
        db,
        &format!("auto_retry_{task}"),
        if enabled { "1" } else { "0" },
    )?;
    if !enabled {
        store_pending(db, task, None)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_is_bounded_and_server_delay_takes_precedence() {
        let now = 200;
        let first = Pending::default()
            .after_failure(now, Duration::ZERO)
            .unwrap();
        assert!((223..=238).contains(&first.next_at));
        let next = first
            .after_failure(now, Duration::from_millis(150_001))
            .unwrap();
        assert_eq!(next.next_at, 351);
        assert_eq!(next.attempts, 2);
        assert!(
            Pending {
                attempts: 5,
                next_at: now
            }
            .after_failure(now, Duration::ZERO)
            .is_none()
        );
    }
}

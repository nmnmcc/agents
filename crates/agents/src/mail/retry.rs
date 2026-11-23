use anyhow::{Context, Result, ensure};
use schemars::JsonSchema;
use serde::Deserialize;
use std::time::{SystemTime, UNIX_EPOCH};

const HEADER: &str = "X-Agents-Queue: ";

const fn min_default() -> u64 {
    300
}

const fn max_default() -> u64 {
    4000
}

const fn lifetime_default() -> u64 {
    5 * 24 * 60 * 60
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    #[serde(default = "min_default")]
    #[schemars(range(min = 1))]
    pub minimal_backoff_time: u64,
    #[serde(default = "max_default")]
    #[schemars(range(min = 1))]
    pub maximal_backoff_time: u64,
    #[serde(default = "lifetime_default")]
    pub maximal_queue_lifetime: u64,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            minimal_backoff_time: min_default(),
            maximal_backoff_time: max_default(),
            maximal_queue_lifetime: lifetime_default(),
        }
    }
}

impl Policy {
    fn check(&self) -> Result<()> {
        ensure!(
            self.minimal_backoff_time > 0 && self.minimal_backoff_time <= self.maximal_backoff_time,
            "require 0 < minimal_backoff_time <= maximal_backoff_time"
        );
        Ok(())
    }

    pub fn expired(&self, created: u64, now: u64) -> bool {
        self.maximal_queue_lifetime != 0
            && now.saturating_sub(created) >= self.maximal_queue_lifetime
    }

    pub fn next_due(&self, created: u64, now: u64) -> Option<u64> {
        if self.maximal_queue_lifetime == 0 || self.expired(created, now) {
            return None;
        }
        // Postfix qmgr uses the age of the message, not the last delay.
        let delay = now
            .saturating_sub(created)
            .clamp(self.minimal_backoff_time, self.maximal_backoff_time);
        Some(
            now.saturating_add(delay)
                .min(created.saturating_add(self.maximal_queue_lifetime)),
        )
    }
}

pub fn decode<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Policy, D::Error> {
    let policy = Policy::deserialize(deserializer)?;
    policy.check().map_err(serde::de::Error::custom)?;
    Ok(policy)
}

pub fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

pub struct QueueEntry<'a> {
    pub created: u64,
    pub due: u64,
    pub message: &'a [u8],
    pub recipient: Option<&'a str>,
}

impl<'a> QueueEntry<'a> {
    pub fn new(message: &'a [u8], now: u64, recipient: &'a str) -> Self {
        Self {
            created: now,
            due: now,
            message,
            recipient: Some(recipient),
        }
    }

    pub fn read(raw: &'a [u8]) -> Result<Self> {
        let line_end = raw
            .iter()
            .position(|&b| b == b'\n')
            .context("missing queue header")?;
        let header = std::str::from_utf8(&raw[..line_end])?.trim_end_matches('\r');
        let (created, due) = header
            .strip_prefix(HEADER)
            .and_then(|text| text.split_once(' '))
            .context("invalid queue header")?;
        let (due, recipient) = due
            .split_once(' ')
            .map_or((due, None), |(due, to)| (due, Some(to)));
        let (created, due) = (created.parse()?, due.parse()?);
        ensure!(created <= due, "queue time is later than retry time");
        Ok(Self {
            created,
            due,
            message: &raw[line_end + 1..],
            recipient,
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut raw = format!("{HEADER}{} {}", self.created, self.due).into_bytes();
        if let Some(to) = self.recipient {
            raw.push(b' ');
            raw.extend_from_slice(to.as_bytes());
        }
        raw.extend_from_slice(b"\r\n");
        raw.extend_from_slice(self.message);
        raw
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postfix_queue_age_backoff_with_lifetime() {
        let policy = Policy::default();
        policy.check().unwrap();
        assert_eq!(policy.next_due(100, 100), Some(400));
        assert_eq!(policy.next_due(100, 400), Some(700));
        assert_eq!(policy.next_due(100, 700), Some(1300));
        assert_eq!(policy.next_due(100, 9100), Some(13100));
        let deadline = 100 + policy.maximal_queue_lifetime;
        assert_eq!(policy.next_due(100, deadline - 1), Some(deadline));
        assert_eq!(policy.next_due(100, deadline), None);
    }

    #[test]
    fn queue_copy_preserves_exact_smtp_payload() {
        let message = b"Message-ID: <id@example.test>\r\n\r\nBody\r\n";
        let mut entry = QueueEntry::new(message, 20, "hidden@example.test");
        entry.due = 40;
        let encoded = entry.encode();
        let decoded = QueueEntry::read(&encoded).unwrap();
        assert_eq!(decoded.recipient, Some("hidden@example.test"));
        assert_eq!(
            (decoded.created, decoded.due, decoded.message),
            (20, 40, &message[..])
        );
    }
}

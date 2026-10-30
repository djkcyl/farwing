//! Server egress: what the relay and the forwarder send for sessions, who
//! pays for it, and (later) its limits (`docs/protocol/egress-policy.md`).

pub mod ledger;
pub mod meter;

use crate::db::Db;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub use ledger::{LOCAL, Ledger};
pub use meter::{Meters, SessionGauge, Usage};

/// Whose egress a session's is (§2): an account, or the guests' common one.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Payer {
    User(String),
    Guests,
}

impl Payer {
    /// The controller's account; a guest's sessions are the host owner's,
    /// and with no account on either side the guests'.
    pub fn of(controller_user: &str, host_owner: Option<&str>) -> Self {
        if !controller_user.is_empty() {
            Self::User(controller_user.to_owned())
        } else {
            host_owner
                .filter(|u| !u.is_empty())
                .map_or(Self::Guests, |u| Self::User(u.to_owned()))
        }
    }

    /// As stored: the user ID, empty for the guests.
    pub fn key(&self) -> &str {
        match self {
            Self::User(id) => id,
            Self::Guests => "",
        }
    }

    pub fn from_key(key: String) -> Self {
        if key.is_empty() { Self::Guests } else { Self::User(key) }
    }
}

/// A calendar month (UTC): its `YYYY-MM` key and bounds in Unix seconds.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Month {
    pub key: String,
    pub start: i64,
    pub end: i64,
}

/// Days since 1970-01-01 of a civil date (proleptic Gregorian).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil year and month of a day since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m)
}

impl Month {
    /// The month that contains `unix_s`.
    pub fn at(unix_s: i64) -> Self {
        let (y, m) = civil_from_days(unix_s.div_euclid(86_400));
        let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
        Self {
            key: format!("{y:04}-{m:02}"),
            start: days_from_civil(y, m, 1) * 86_400,
            end: days_from_civil(ny, nm, 1) * 86_400,
        }
    }

    pub fn now() -> Self {
        Self::at(crate::db::now_s())
    }
}

/// The control node's egress service: this server's gauges and the
/// account of every node's.
pub struct Egress {
    meters: Arc<Meters>,
    pub ledger: Ledger,
}

/// How often stored monthly totals are brought up to date.
const STORE_EVERY: Duration = Duration::from_secs(60);

impl Egress {
    /// The service, with this month's totals from `db`.
    pub fn load(db: &Db) -> anyhow::Result<Arc<Self>> {
        let month = Month::now();
        let key = month.key.clone();
        let stored = db.with(move |c| crate::db::egress_month(c, &key))?;
        Ok(Arc::new(Self {
            meters: Meters::new(),
            ledger: Ledger::new(
                month,
                stored
                    .into_iter()
                    .map(|(payer, bytes)| (Payer::from_key(payer), bytes))
                    .collect(),
            ),
        }))
    }

    /// This server's gauges (its relay and forwarder count into them).
    pub fn meters(&self) -> Arc<Meters> {
        Arc::clone(&self.meters)
    }

    /// An edge node's report.
    pub fn report(&self, node: &str, usage: &Usage) {
        self.ledger.report(node, usage, Instant::now(), &Month::now());
    }

    /// Counts this server's egress each second and stores the monthly
    /// totals every minute, until the process ends.
    pub async fn run(self: Arc<Self>, db: Db) {
        let mut ticks = tokio::time::interval(Duration::from_secs(1));
        let mut stored = Instant::now();
        loop {
            ticks.tick().await;
            self.ledger
                .report(LOCAL, &self.meters.usage(), Instant::now(), &Month::now());
            if stored.elapsed() >= STORE_EVERY {
                stored = Instant::now();
                self.store(&db).await;
            }
        }
    }

    /// Writes the monthly bytes not yet stored.
    pub async fn store(&self, db: &Db) {
        let rows = self.ledger.take_pending();
        if rows.is_empty() {
            return;
        }
        let keyed: Vec<(String, String, u64)> = rows
            .iter()
            .map(|(m, p, b)| (m.clone(), p.key().to_owned(), *b))
            .collect();
        if let Err(error) = db.call(move |c| crate::db::egress_add(c, &keyed)).await {
            tracing::warn!(error = %format!("{error:#}"), "egress totals not stored");
            self.ledger.restore_pending(rows);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn months_have_their_bounds() {
        // 2026-10-30T12:00:00Z
        let october = Month::at(1_793_361_600);
        assert_eq!(october.key, "2026-10");
        assert_eq!(october.start, 1_790_812_800);
        assert_eq!(october.end, 1_793_491_200);
        assert_eq!(Month::at(october.end).key, "2026-11");
        assert_eq!(Month::at(october.end - 1).key, "2026-10");
        let december = Month::at(1_798_761_600 - 1);
        assert_eq!(december.key, "2026-12");
        assert_eq!(Month::at(december.end).key, "2027-01");
    }

    #[test]
    fn the_controller_pays_else_the_host_owner_else_the_guests() {
        assert_eq!(Payer::of("a", Some("b")), Payer::User("a".into()));
        assert_eq!(Payer::of("", Some("b")), Payer::User("b".into()));
        assert_eq!(Payer::of("", None), Payer::Guests);
        assert_eq!(Payer::from_key(String::new()), Payer::Guests);
    }
}

//! Connection records (`docs/connection-records.md`): one row per visit, a
//! controller's viewer window or file or port session, from the first
//! admission to the end; reconnections reopen the same row. Attempts that
//! were not admitted get rows of their own. The two sides add their
//! summaries (`RecordReport`) when the visit ends; connections without the
//! server arrive only as such summaries.

use crate::db::{R, now_s};
use farwing_protocol::Platform;
use farwing_protocol::api::{
    ConnectionRecord, HostReport, Purpose, RecordOutcome, RecordParty, RecordReport, Relation,
    ViewerReport,
};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// A visit whose last session ended this long ago is over: a new session
/// with its ID starts a new record. The viewer gives up reconnecting after
/// five minutes.
const RECONNECT_WITHIN_S: i64 = 360;

/// A session the server admitted, or an attempt it refused.
#[derive(Clone, Debug, Default)]
pub struct Opened {
    pub session_id: String,
    /// The controller's visit; empty: the session ID.
    pub visit: String,
    pub host: RecordParty,
    pub controller: RecordParty,
    pub relation: Relation,
    pub admission: String,
    pub purpose: Purpose,
    pub ip: String,
    pub at: i64,
}

/// Who asks for records.
#[derive(Clone, Debug, Default)]
pub struct Filter {
    /// Records where this account is either side (`None`: all, admins).
    pub user: Option<String>,
    /// Records where this device is either side.
    pub device: Option<String>,
    pub before: Option<i64>,
    pub limit: u32,
}

/// Why a report is refused.
#[derive(Debug, PartialEq, Eq)]
pub enum ReportError {
    /// No record of this visit, and the report does not describe a direct
    /// connection.
    NotFound,
    /// The reporting device is neither side.
    NotAParty,
}

/// The host side of a record.
pub fn host_party(d: &crate::db::DeviceRow) -> RecordParty {
    RecordParty {
        device_id: d.id.clone(),
        name: if d.alias.is_empty() { d.name.clone() } else { d.alias.clone() },
        user_id: d.user_id.clone().unwrap_or_default(),
        platform: d.platform,
        ..RecordParty::default()
    }
}

/// The controller side of a record.
pub fn controller_party(c: &farwing_protocol::realtime::ControllerInfo) -> RecordParty {
    RecordParty {
        device_id: c.device_id.clone(),
        name: c.alias.clone(),
        user_id: c.user_id.clone(),
        username: c.username.clone(),
        nickname: c.nickname.clone(),
        platform: c.platform,
    }
}

fn text<T: Serialize>(value: T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(s)) => s,
        _ => String::new(),
    }
}

fn parse<T: DeserializeOwned + Default>(text: &str) -> T {
    serde_json::from_value(serde_json::Value::String(text.to_owned())).unwrap_or_default()
}

fn json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

fn visit_of(o: &Opened) -> &str {
    if o.visit.is_empty() { &o.session_id } else { &o.visit }
}

fn insert(c: &Connection, o: &Opened, visit: &str, outcome: RecordOutcome, direct: bool) -> R<i64> {
    let ended = (outcome != RecordOutcome::Open).then_some(o.at);
    c.execute(
        "INSERT INTO records (visit, session_id, host_device_id, host_user_id, host_name,
             host_platform, controller_device_id, controller_user_id, controller_name,
             controller_username, controller_nickname, controller_platform, relation,
             admission, purpose, direct, ip, started_at, ended_at, outcome)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
             ?18, ?19, ?20)",
        params![
            visit,
            o.session_id,
            o.host.device_id,
            o.host.user_id,
            o.host.name,
            o.host.platform.as_str(),
            o.controller.device_id,
            o.controller.user_id,
            o.controller.name,
            o.controller.username,
            o.controller.nickname,
            o.controller.platform.as_str(),
            text(o.relation),
            o.admission,
            text(o.purpose),
            direct,
            o.ip,
            o.at,
            ended,
            outcome.as_str(),
        ],
    )?;
    Ok(c.last_insert_rowid())
}

/// The latest record of a visit: `(seq, outcome, ended_at, session_id)`.
fn latest(
    c: &Connection,
    controller: &str,
    host: &str,
    visit: &str,
) -> R<Option<(i64, String, Option<i64>, String)>> {
    c.query_row(
        "SELECT seq, outcome, ended_at, session_id FROM records
         WHERE controller_device_id = ?1 AND host_device_id = ?2 AND visit = ?3
         ORDER BY seq DESC LIMIT 1",
        params![controller, host, visit],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )
    .optional()
}

/// A session was admitted: a new record, or the visit's record reopened.
pub fn open(c: &Connection, o: &Opened) -> R<()> {
    let visit = visit_of(o);
    match latest(c, &o.controller.device_id, &o.host.device_id, visit)? {
        Some((seq, outcome, ended, sid))
            if outcome == "open"
                || (outcome == "ended" && ended.is_some_and(|e| o.at - e <= RECONNECT_WITHIN_S)) =>
        {
            if sid != o.session_id {
                c.execute(
                    "UPDATE records SET session_id = ?2, outcome = 'open', ended_at = NULL,
                         end_reason = '', sessions = sessions + 1, ip = ?3
                     WHERE seq = ?1",
                    params![seq, o.session_id, o.ip],
                )?;
            }
        }
        _ => {
            insert(c, o, visit, RecordOutcome::Open, false)?;
        }
    }
    Ok(())
}

/// An attempt that was not admitted: a record of its own.
pub fn refused(c: &Connection, o: &Opened, outcome: RecordOutcome) -> R<()> {
    insert(c, o, &crate::crypto::new_uuid(), outcome, false).map(|_| ())
}

/// A session ended; its visit's record ends unless a newer session took over.
pub fn end(c: &Connection, session_id: &str, reason: &str) -> R<()> {
    c.execute(
        "UPDATE records SET outcome = 'ended', ended_at = ?2, end_reason = ?3
         WHERE session_id = ?1 AND outcome = 'open'",
        params![session_id, now_s(), reason],
    )?;
    Ok(())
}

/// Every open record ends (the server starts: its sessions are gone).
pub fn end_all(c: &Connection, reason: &str) -> R<usize> {
    c.execute(
        "UPDATE records SET outcome = 'ended', ended_at = ?1, end_reason = ?2
         WHERE outcome = 'open'",
        params![now_s(), reason],
    )
}

/// A side's summary. `reporter` is the reporting device and `user` its
/// account. A direct connection without a record gets one; its other side's
/// account is filled in only when that side reports too.
pub fn report(
    c: &Connection,
    reporter: &RecordParty,
    r: &RecordReport,
) -> Result<(), ReportOrDb> {
    let as_host = reporter.device_id == r.host_device_id;
    let as_controller = reporter.device_id == r.controller_device_id;
    if !as_host && !as_controller {
        return Err(ReportError::NotAParty.into());
    }
    let seq = match latest(c, &r.controller_device_id, &r.host_device_id, &r.visit)? {
        Some((seq, ..)) => seq,
        None => {
            let Some(d) = &r.direct else {
                return Err(ReportError::NotFound.into());
            };
            let party = |device_id: &str, name: &str, platform| RecordParty {
                device_id: device_id.to_owned(),
                name: name.to_owned(),
                platform,
                ..RecordParty::default()
            };
            let mut o = Opened {
                session_id: String::new(),
                visit: r.visit.clone(),
                host: party(&r.host_device_id, &d.host_name, Platform::Windows),
                controller: party(&r.controller_device_id, &d.controller_name, d.controller_platform),
                relation: d.relation,
                admission: d.admission.clone(),
                purpose: d.purpose,
                ip: String::new(),
                at: d.started_at,
            };
            if as_host {
                o.host = reporter.clone();
            } else {
                o.controller = reporter.clone();
            }
            let seq = insert(c, &o, &r.visit, RecordOutcome::Ended, true)?;
            c.execute(
                "UPDATE records SET ended_at = ?2, end_reason = 'direct' WHERE seq = ?1",
                params![seq, d.ended_at.max(d.started_at)],
            )?;
            seq
        }
    };
    // The reporter's own side: its account and name as it says.
    if as_host {
        c.execute(
            "UPDATE records SET host_user_id = CASE WHEN direct = 1 THEN ?2 ELSE host_user_id END,
                 host = COALESCE(?3, host)
             WHERE seq = ?1",
            params![seq, reporter.user_id, r.host.as_ref().map(json)],
        )?;
    }
    if as_controller {
        c.execute(
            "UPDATE records SET
                 controller_user_id = CASE WHEN direct = 1 THEN ?2 ELSE controller_user_id END,
                 controller_username = CASE WHEN direct = 1 THEN ?3 ELSE controller_username END,
                 controller_nickname = CASE WHEN direct = 1 THEN ?4 ELSE controller_nickname END,
                 viewer = COALESCE(?5, viewer)
             WHERE seq = ?1",
            params![
                seq,
                reporter.user_id,
                reporter.username,
                reporter.nickname,
                r.viewer.as_ref().map(json)
            ],
        )?;
    }
    Ok(())
}

/// [`report`]'s errors.
#[derive(Debug)]
pub enum ReportOrDb {
    Refused(ReportError),
    Db(rusqlite::Error),
}

impl From<ReportError> for ReportOrDb {
    fn from(e: ReportError) -> Self {
        Self::Refused(e)
    }
}

impl From<rusqlite::Error> for ReportOrDb {
    fn from(e: rusqlite::Error) -> Self {
        Self::Db(e)
    }
}

const COLUMNS: &str = "seq, host_device_id, host_user_id, host_name, host_platform,
     controller_device_id, controller_user_id, controller_name, controller_username,
     controller_nickname, controller_platform, relation, admission, purpose, direct, ip,
     started_at, ended_at, outcome, end_reason, sessions, viewer, host";

fn from_row(r: &Row<'_>) -> R<ConnectionRecord> {
    let platform = |s: String| Platform::parse(&s).unwrap_or_default();
    let viewer: Option<String> = r.get(21)?;
    let host: Option<String> = r.get(22)?;
    Ok(ConnectionRecord {
        id: r.get(0)?,
        host: RecordParty {
            device_id: r.get(1)?,
            user_id: r.get(2)?,
            name: r.get(3)?,
            platform: platform(r.get(4)?),
            ..RecordParty::default()
        },
        controller: RecordParty {
            device_id: r.get(5)?,
            user_id: r.get(6)?,
            name: r.get(7)?,
            username: r.get(8)?,
            nickname: r.get(9)?,
            platform: platform(r.get(10)?),
        },
        relation: parse(&r.get::<_, String>(11)?),
        admission: r.get(12)?,
        purpose: parse(&r.get::<_, String>(13)?),
        direct: r.get(14)?,
        ip: r.get(15)?,
        started_at: r.get(16)?,
        ended_at: r.get(17)?,
        outcome: RecordOutcome::parse(&r.get::<_, String>(18)?),
        end_reason: r.get(19)?,
        reconnects: r.get::<_, u32>(20)?.saturating_sub(1),
        viewer: viewer.and_then(|v| serde_json::from_str::<ViewerReport>(&v).ok()),
        host_report: host.and_then(|v| serde_json::from_str::<HostReport>(&v).ok()),
    })
}

/// Newest first.
pub fn list(c: &Connection, f: &Filter) -> R<Vec<ConnectionRecord>> {
    let mut st = c.prepare(&format!(
        "SELECT {COLUMNS} FROM records
         WHERE (?1 IS NULL OR host_user_id = ?1 OR controller_user_id = ?1)
           AND (?2 IS NULL OR host_device_id = ?2 OR controller_device_id = ?2)
           AND (?3 IS NULL OR seq < ?3)
         ORDER BY seq DESC LIMIT ?4"
    ))?;
    st.query_map(
        params![f.user, f.device, f.before, f.limit.clamp(1, 200)],
        from_row,
    )?
    .collect()
}

/// Drop records that started more than `keep_s` ago.
pub fn prune(c: &Connection, keep_s: i64) -> R<usize> {
    c.execute(
        "DELETE FROM records WHERE started_at < ?1 AND outcome != 'open'",
        [now_s() - keep_s],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use farwing_protocol::api::{DirectRecord, FileOutcome, FileRecord};

    fn db() -> Connection {
        let dir = std::env::temp_dir().join(format!("farwing-records-{}", crate::crypto::new_uuid()));
        std::fs::create_dir_all(&dir).unwrap();
        crate::db::open_connection(&dir.join("farwing.db")).unwrap()
    }

    fn party(device: &str, user: &str) -> RecordParty {
        RecordParty {
            device_id: device.into(),
            name: format!("{device} name"),
            user_id: user.into(),
            username: user.into(),
            platform: Platform::Windows,
            ..RecordParty::default()
        }
    }

    fn opened(sid: &str, visit: &str, at: i64) -> Opened {
        Opened {
            session_id: sid.into(),
            visit: visit.into(),
            host: party("host", "alice"),
            controller: party("ctrl", "bob"),
            relation: Relation::Friend,
            purpose: Purpose::Desktop,
            ip: "198.51.100.7".into(),
            at,
            ..Opened::default()
        }
    }

    fn all(c: &Connection) -> Vec<ConnectionRecord> {
        list(c, &Filter { limit: 50, ..Filter::default() }).unwrap()
    }

    #[test]
    fn reconnections_stay_one_record_and_a_new_visit_makes_another() {
        let c = db();
        let now = now_s();
        open(&c, &opened("s1", "v1", now - 100)).unwrap();
        end(&c, "s1", "controller_left").unwrap();
        open(&c, &opened("s2", "v1", now)).unwrap();
        let records = all(&c);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].outcome, RecordOutcome::Open);
        assert_eq!(records[0].reconnects, 1);
        assert_eq!(records[0].started_at, now - 100);
        // The replaced session ends after its successor took the record.
        end(&c, "s1", "replaced").unwrap();
        assert_eq!(all(&c)[0].outcome, RecordOutcome::Open);
        end(&c, "s2", "controller_left").unwrap();
        open(&c, &opened("s3", "v2", now)).unwrap();
        let records = all(&c);
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].outcome, RecordOutcome::Ended);
        assert_eq!(records[1].end_reason, "controller_left");
    }

    #[test]
    fn a_visit_long_over_starts_again() {
        let c = db();
        let now = now_s();
        open(&c, &opened("s1", "v1", now - 1000)).unwrap();
        c.execute("UPDATE records SET outcome = 'ended', ended_at = ?1", [now - 900]).unwrap();
        open(&c, &opened("s2", "v1", now)).unwrap();
        assert_eq!(all(&c).len(), 2);
    }

    #[test]
    fn each_side_sees_its_records_and_reports_its_part() {
        let c = db();
        let now = now_s();
        open(&c, &opened("s1", "v1", now)).unwrap();
        refused(&c, &opened("", "v9", now), RecordOutcome::CodeInvalid).unwrap();
        let mine = |user: &str| {
            list(&c, &Filter { user: Some(user.into()), limit: 50, ..Filter::default() }).unwrap()
        };
        assert_eq!(mine("alice").len(), 2);
        assert_eq!(mine("bob").len(), 2);
        assert_eq!(mine("carol").len(), 0);
        let host_report = HostReport {
            controlled: true,
            files: vec![FileRecord {
                name: "a.txt".into(),
                count: 1,
                bytes: 10,
                upload: true,
                outcome: FileOutcome::Done,
            }],
            ..HostReport::default()
        };
        let report_of = |viewer: Option<ViewerReport>, host: Option<HostReport>| RecordReport {
            visit: "v1".into(),
            host_device_id: "host".into(),
            controller_device_id: "ctrl".into(),
            viewer,
            host,
            ..RecordReport::default()
        };
        // Each side writes only its own part.
        report(&c, &party("host", "alice"), &report_of(Some(ViewerReport::default()), Some(host_report.clone()))).unwrap();
        let viewer = ViewerReport { route: "p2p".into(), fps: 59.5, ..ViewerReport::default() };
        report(&c, &party("ctrl", "bob"), &report_of(Some(viewer.clone()), None)).unwrap();
        let record = mine("bob").into_iter().find(|r| r.outcome == RecordOutcome::Open).unwrap();
        assert_eq!(record.host_report, Some(host_report));
        assert_eq!(record.viewer, Some(viewer));
        assert!(matches!(
            report(&c, &party("other", "carol"), &report_of(None, None)),
            Err(ReportOrDb::Refused(ReportError::NotAParty))
        ));
    }

    #[test]
    fn a_direct_connection_is_recorded_from_its_reports() {
        let c = db();
        let now = now_s();
        let r = RecordReport {
            visit: "d1".into(),
            host_device_id: "host".into(),
            controller_device_id: "ctrl".into(),
            direct: Some(DirectRecord {
                started_at: now - 60,
                ended_at: now,
                relation: Relation::Trusted,
                host_name: "Host".into(),
                controller_name: "Ctrl".into(),
                ..DirectRecord::default()
            }),
            ..RecordReport::default()
        };
        assert!(matches!(
            report(&c, &party("ctrl", "bob"), &RecordReport { direct: None, ..r.clone() }),
            Err(ReportOrDb::Refused(ReportError::NotFound))
        ));
        report(&c, &party("host", "alice"), &r).unwrap();
        let mine = |user: &str| {
            list(&c, &Filter { user: Some(user.into()), limit: 50, ..Filter::default() }).unwrap()
        };
        // Only the side that reported sees it until the other side does.
        assert_eq!(mine("alice").len(), 1);
        assert_eq!(mine("bob").len(), 0);
        report(&c, &party("ctrl", "bob"), &r).unwrap();
        let record = &mine("bob")[0];
        assert!(record.direct);
        assert_eq!(record.ended_at, Some(now));
        assert_eq!(record.controller.name, "Ctrl");
        assert_eq!(all(&c).len(), 1);
    }
}

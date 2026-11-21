//! The desk's requests (agent proposals), persisted so the audit outlives a
//! restart: the person can see days later what was asked, what they decided
//! and the verdict. A proposal is still only ever run by the person.

use rusqlite::{params, Connection};
use zeron_proto::trading::{ProposalStatus, TradingProposal};

/// How many requests the store keeps for the audit. The desk itself shows the
/// newest 50; a few more survive for the brief.
pub const KEEP: usize = 100;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn status_str(status: ProposalStatus) -> &'static str {
    match status {
        ProposalStatus::Pending => "pending",
        ProposalStatus::Rejected => "rejected",
        ProposalStatus::Done => "done",
        ProposalStatus::Failed => "failed",
    }
}

fn status_from_str(text: &str) -> ProposalStatus {
    match text {
        "rejected" => ProposalStatus::Rejected,
        "done" => ProposalStatus::Done,
        "failed" => ProposalStatus::Failed,
        _ => ProposalStatus::Pending,
    }
}

/// Every stored request, newest first, at most `limit`.
pub fn list(conn: &Connection, limit: usize) -> rusqlite::Result<Vec<TradingProposal>> {
    let mut stmt = conn.prepare(
        "SELECT id, action, reason, chat_id, status, created_at, resolved_at, result \
         FROM trading_proposal ORDER BY created_at DESC, id DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit as i64], |row| {
        let action: String = row.get(1)?;
        let action = serde_json::from_str(&action).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
        let status: String = row.get(4)?;
        Ok(TradingProposal {
            id: row.get(0)?,
            action,
            reason: row.get(2)?,
            chat_id: row.get(3)?,
            status: status_from_str(&status),
            created_at: row.get(5)?,
            resolved_at: row.get(6)?,
            result: row.get(7)?,
        })
    })?;
    rows.collect()
}

/// Inserts a new request or replaces the stored one with the same id (its
/// outcome, once the person answers).
pub fn upsert(conn: &Connection, proposal: &TradingProposal) -> rusqlite::Result<()> {
    let action = serde_json::to_string(&proposal.action)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    conn.execute(
        "INSERT INTO trading_proposal \
         (id, action, reason, chat_id, status, created_at, resolved_at, result, time_created, time_updated) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?6, ?6) \
         ON CONFLICT(id) DO UPDATE SET status=?5, resolved_at=?7, result=?8, time_updated=?9",
        params![
            proposal.id,
            action,
            proposal.reason,
            proposal.chat_id,
            status_str(proposal.status),
            proposal.created_at,
            proposal.resolved_at,
            proposal.result,
            now_ms(),
        ],
    )?;
    Ok(())
}

/// Drops all but the newest `keep` requests.
pub fn prune(conn: &Connection, keep: usize) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM trading_proposal WHERE id NOT IN (\
         SELECT id FROM trading_proposal ORDER BY created_at DESC, id DESC LIMIT ?1)",
        [keep as i64],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::trading::ProposalAction;

    fn proposal(id: &str, created_at: i64) -> TradingProposal {
        TradingProposal {
            id: id.into(),
            action: ProposalAction::CloseAll,
            reason: format!("petición {id}"),
            chat_id: Some("chat-1".into()),
            status: ProposalStatus::Pending,
            created_at,
            resolved_at: None,
            result: None,
        }
    }

    #[test]
    fn proposals_roundtrip_their_outcome_and_prune_the_oldest() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::store::MIGRATION_SQL).unwrap();
        let mut done = proposal("p1", 10);
        done.status = ProposalStatus::Done;
        done.resolved_at = Some(20);
        done.result = Some("3 posiciones cerradas.".into());
        upsert(&conn, &done).unwrap();
        upsert(&conn, &proposal("p2", 30)).unwrap();
        assert_eq!(list(&conn, 10).unwrap(), vec![proposal("p2", 30), done.clone()]);

        // The person's answer replaces the pending row.
        let mut rejected = proposal("p2", 30);
        rejected.status = ProposalStatus::Rejected;
        rejected.resolved_at = Some(40);
        rejected.result = Some("Rechazada en la mesa.".into());
        upsert(&conn, &rejected).unwrap();
        assert_eq!(list(&conn, 10).unwrap(), vec![rejected, done]);

        // The audit is bounded: the newest stay.
        for ix in 0..5 {
            upsert(&conn, &proposal(&format!("q{ix}"), 100 + ix)).unwrap();
        }
        assert_eq!(prune(&conn, 3).unwrap(), 4);
        let kept: Vec<String> = list(&conn, 10).unwrap().into_iter().map(|p| p.id).collect();
        assert_eq!(kept, ["q4", "q3", "q2"]);
    }
}

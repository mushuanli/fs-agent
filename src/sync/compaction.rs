//! Compact replaceable discovery indexes without discarding identities or history.
use super::{model::*, policy, service::SyncService, store::metadata as m};
use rusqlite::Connection;
use serde_json::Value;

impl SyncService {
    pub(super) fn check_change_floor(
        &self,
        db: &Connection,
        project: &str,
        sequence: u64,
    ) -> Result<()> {
        if sequence < self.project(db, project, false)?.change_floor {
            return Err(Error::new("CURSOR_EXPIRED", 410));
        }
        Ok(())
    }
    pub(super) fn compact(&self, db: &mut Connection) -> Result<usize> {
        let tx = self.transaction(db)?;
        let cutoff = policy::change_cutoff(
            self.time(),
            self.config.change_retention_seconds,
            self.config.read_pin_seconds,
        );
        let mut removed = 0;
        for (_, mut p) in m::list::<Project>(&tx, "", "project")? {
            let budget = 2000 - removed;
            if budget == 0 {
                break;
            }
            let (floor, events) = compaction_floor(&tx, &p, cutoff, budget.min(1000))?;
            removed += compact_catalog(&tx, &p.project_id, floor, (budget - events).min(1000))?;
            removed += tx.execute(
                "DELETE FROM records WHERE scope=?1 AND kind='change' AND key<=?2",
                (&p.project_id, format!("{floor:020}")),
            )?;
            p.change_floor = floor;
            m::put(&tx, "", "project", &p.project_id, &p)?;
        }
        self.commit(tx)?;
        Ok(removed)
    }
}
fn compaction_floor(
    db: &Connection,
    p: &Project,
    cutoff: u64,
    limit: usize,
) -> Result<(u64, usize)> {
    let rows = m::range::<Value>(
        db,
        &p.project_id,
        "change",
        &format!("{:020}", p.change_floor),
        &format!("{:020}", p.sequence),
        limit,
    )?;
    let mut floor = p.change_floor;
    let mut count = 0;
    for (key, event) in rows {
        // Unknown legacy timestamps are a conservative barrier, never guessed.
        if !event["recordedAt"]
            .as_u64()
            .is_some_and(|time| time <= cutoff)
        {
            break;
        }
        floor = policy::number(key.trim_start_matches('0'))?;
        count += 1;
    }
    Ok((floor, count))
}
fn compact_catalog(db: &Connection, project: &str, floor: u64, limit: usize) -> Result<usize> {
    // Keep the latest snapshot per member at the floor, including deleted members.
    Ok(db.execute(
        "WITH obsolete AS (
        SELECT key, ROW_NUMBER() OVER (
          PARTITION BY json_extract(value,'$.datasetId') ORDER BY key DESC) AS position
        FROM records WHERE scope=?1 AND kind='catalog' AND key<=?2)
        DELETE FROM records WHERE scope=?1 AND kind='catalog' AND key IN (
          SELECT key FROM obsolete WHERE position>1 LIMIT ?3)",
        (project, format!("{floor:020}/~"), limit),
    )?)
}

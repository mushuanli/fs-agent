//! Native hard deletion validates the entire spawned tree before its commit point.
use super::{
    bridge::Bridge,
    codex::{Codex, SOURCE_KINDS},
    service::string,
};
use crate::core::error::Error;
use serde_json::{json, Value};
use std::collections::HashSet;

impl Codex {
    pub(super) async fn delete_native(
        &self,
        bridge: &Bridge,
        thread: Value,
    ) -> Result<Value, Error> {
        self.deletable(&thread).await?;
        let id = string(&thread, "id")?;
        let mut ids = vec![id.to_owned()];
        for archived in [false, true] {
            ids.extend(
                self.management_descendants(bridge, id, archived, true)
                    .await?,
            );
        }
        // Native deletion owns rollout and metadata cleanup; never edit its database.
        bridge
            .persistent_call("thread/delete", json!({"threadId":id}))
            .await?;
        let mut owned = self.owned.lock().await;
        for id in &ids {
            owned.remove(id);
        }
        Ok(json!({"deletedSessionIds":ids}))
    }
    pub(super) async fn archivable(&self, thread: &Value) -> Result<(), Error> {
        if !thread["cwd"]
            .as_str()
            .is_some_and(|cwd| self.authorized(cwd))
        {
            return Err(Error::forbidden("EACCES"));
        }
        match thread["status"]["type"].as_str() {
            Some("idle") => self.require_owned(string(thread, "id")?).await,
            Some("notLoaded") => Ok(()),
            _ => Err(Error::busy()),
        }
    }
    async fn deletable(&self, thread: &Value) -> Result<(), Error> {
        if !thread["cwd"]
            .as_str()
            .is_some_and(|cwd| self.authorized(cwd))
        {
            return Err(Error::forbidden("EACCES"));
        }
        match thread["status"]["type"].as_str() {
            Some("idle") => self.require_owned(string(thread, "id")?).await,
            Some("notLoaded")
                if thread["archived"] == true
                    || thread["path"]
                        .as_str()
                        .is_some_and(|path| path.contains("/archived_sessions/")) =>
            {
                Ok(())
            }
            _ => Err(Error::busy()),
        }
    }
    pub(super) async fn management_descendants(
        &self,
        bridge: &Bridge,
        id: &str,
        archived: bool,
        deleting: bool,
    ) -> Result<Vec<String>, Error> {
        let mut cursor = Value::Null;
        let mut seen = HashSet::new();
        let mut ids = HashSet::new();
        for _ in 0..82 {
            let page = bridge
                .call(
                    "thread/list",
                    json!({"ancestorThreadId":id,
                "archived":archived,"cursor":cursor,"limit":100,"sourceKinds":SOURCE_KINDS}),
                )
                .await?;
            self.management_rows(&page, id, deleting, &mut ids).await?;
            cursor = page["nextCursor"].clone();
            if cursor.is_null() {
                return Ok(ids.into_iter().collect());
            }
            if !cursor.is_string() || !seen.insert(cursor.to_string()) {
                return Err(Error::invalid());
            }
        }
        Err(Error::too_many("HARNESS_DELETE_TREE_LIMIT"))
    }
    async fn management_rows(
        &self,
        page: &Value,
        id: &str,
        deleting: bool,
        ids: &mut HashSet<String>,
    ) -> Result<(), Error> {
        let rows = page["data"].as_array().ok_or_else(Error::internal)?;
        if rows.len() > 100 {
            return Err(Error::invalid());
        }
        for row in rows {
            if deleting {
                self.deletable(row).await?;
            } else {
                self.archivable(row).await?;
            }
            let child = string(row, "id")?;
            if child == id || !ids.insert(child.to_owned()) {
                return Err(Error::invalid());
            }
        }
        Ok(())
    }
}

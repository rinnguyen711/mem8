pub mod sqlite;

use crate::error::{Mem8Error, Result};
use crate::model::{Memory, MemoryUpdate, NewMemory, SearchHit, SearchQuery};
use async_trait::async_trait;
use chrono::Utc;
use std::sync::Mutex;
use uuid::Uuid;

/// Storage backend for memories.
///
/// Implementations must not leak backend-specific error types; wrap them in
/// `Mem8Error::Store`. This trait is the only place that knows about
/// persistence — `core` above it contains no SQL.
#[async_trait]
pub trait Store: Send + Sync {
    async fn add(&self, new: NewMemory) -> Result<Memory>;
    async fn get(&self, id: Uuid) -> Result<Memory>;
    async fn update(&self, id: Uuid, update: MemoryUpdate) -> Result<Memory>;
    async fn delete(&self, id: Uuid) -> Result<()>;
    async fn search(&self, query: SearchQuery) -> Result<Vec<SearchHit>>;
    /// Every memory, ordered by `created_at` ascending. Used by `mem8 export`.
    async fn all(&self) -> Result<Vec<Memory>>;
}

/// In-memory `Store` used by `core` unit tests. Substring matching stands in
/// for full-text search; ranking fidelity is covered by the contract suite
/// against the real backends.
///
/// Test support only — do not wire this in as a production backend. Its
/// `Mutex` guards are unwrapped without poisoning recovery, so a panic taken
/// while the lock is held would poison it and fail every later call on the
/// same instance. The real backends carry the availability requirement that
/// a store failure must not end the session.
pub struct MemStore {
    rows: Mutex<Vec<Memory>>,
}

impl MemStore {
    pub fn new() -> Self {
        Self { rows: Mutex::new(Vec::new()) }
    }
}

impl Default for MemStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Store for MemStore {
    async fn add(&self, new: NewMemory) -> Result<Memory> {
        let now = Utc::now();
        let memory = Memory {
            id: Uuid::new_v4(),
            project: new.project,
            kind: new.kind,
            content: new.content,
            tags: new.tags,
            created_at: now,
            updated_at: now,
            embedding: None,
        };
        self.rows.lock().unwrap().push(memory.clone());
        Ok(memory)
    }

    async fn get(&self, id: Uuid) -> Result<Memory> {
        self.rows
            .lock()
            .unwrap()
            .iter()
            .find(|m| m.id == id)
            .cloned()
            .ok_or_else(|| Mem8Error::NotFound(id.to_string()))
    }

    async fn update(&self, id: Uuid, update: MemoryUpdate) -> Result<Memory> {
        let mut rows = self.rows.lock().unwrap();
        let row = rows
            .iter_mut()
            .find(|m| m.id == id)
            .ok_or_else(|| Mem8Error::NotFound(id.to_string()))?;

        if let Some(content) = update.content {
            row.content = content;
        }
        if let Some(kind) = update.kind {
            row.kind = kind;
        }
        if let Some(tags) = update.tags {
            row.tags = tags;
        }
        row.updated_at = Utc::now();
        Ok(row.clone())
    }

    async fn delete(&self, id: Uuid) -> Result<()> {
        let mut rows = self.rows.lock().unwrap();
        let before = rows.len();
        rows.retain(|m| m.id != id);
        if rows.len() == before {
            return Err(Mem8Error::NotFound(id.to_string()));
        }
        Ok(())
    }

    async fn search(&self, query: SearchQuery) -> Result<Vec<SearchHit>> {
        let needle = query.text.to_lowercase();
        let rows = self.rows.lock().unwrap();
        let hits = rows
            .iter()
            .filter(|m| query.global || query.project.as_deref() == Some(m.project.as_str()))
            .filter(|m| query.kind.is_none_or(|k| k == m.kind))
            .filter(|m| query.tags.iter().all(|t| m.tags.contains(t)))
            .filter(|m| m.content.to_lowercase().contains(&needle))
            .take(query.limit)
            .map(|m| SearchHit { memory: m.clone(), score: 1.0 })
            .collect();
        Ok(hits)
    }

    async fn all(&self) -> Result<Vec<Memory>> {
        let mut rows = self.rows.lock().unwrap().clone();
        rows.sort_by_key(|m| m.created_at);
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Kind, NewMemory, SearchQuery};

    fn new_memory(project: &str, content: &str) -> NewMemory {
        NewMemory {
            project: project.into(),
            kind: Kind::Decision,
            content: content.into(),
            tags: vec![],
        }
    }

    fn query(text: &str, project: &str) -> SearchQuery {
        SearchQuery {
            text: text.into(),
            project: Some(project.into()),
            global: false,
            kind: None,
            tags: vec![],
            limit: 10,
        }
    }

    #[tokio::test]
    async fn add_then_get_returns_same_content() {
        let store = MemStore::new();
        let added = store.add(new_memory("p1", "use rust")).await.unwrap();
        let fetched = store.get(added.id).await.unwrap();
        assert_eq!(fetched.content, "use rust");
    }

    #[tokio::test]
    async fn get_missing_id_is_not_found() {
        let store = MemStore::new();
        let err = store.get(uuid::Uuid::new_v4()).await.unwrap_err();
        assert!(matches!(err, Mem8Error::NotFound(_)));
    }

    #[tokio::test]
    async fn search_filters_by_project() {
        let store = MemStore::new();
        store.add(new_memory("p1", "use rust")).await.unwrap();
        store.add(new_memory("p2", "use rust")).await.unwrap();

        let hits = store.search(query("rust", "p1")).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].memory.project, "p1");
    }

    #[tokio::test]
    async fn delete_removes_the_memory() {
        let store = MemStore::new();
        let added = store.add(new_memory("p1", "use rust")).await.unwrap();
        store.delete(added.id).await.unwrap();
        assert!(store.get(added.id).await.is_err());
    }
}

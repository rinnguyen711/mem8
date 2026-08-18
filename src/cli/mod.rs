pub mod markdown;

use crate::core::Memory8;
use crate::error::{Mem8Error, Result};
use crate::store::open_from_env;
use std::path::Path;
use std::sync::Arc;

fn io_err(path: &Path) -> impl Fn(std::io::Error) -> Mem8Error + '_ {
    move |source| Mem8Error::Io {
        path: path.display().to_string(),
        source,
    }
}

/// Build the service, loading the embedding model when this build has one.
///
/// One place, so the MCP server and every CLI subcommand agree on whether
/// semantic search is available.
///
/// A model that fails to load is reported and then ignored: mem8 continues
/// keyword-only. Refusing to start because an optional index is unavailable
/// would make the whole memory unreachable over a feature the user may not even
/// use.
pub async fn build_service() -> Result<Memory8> {
    let store = open_from_env().await?;

    #[cfg(feature = "semantic")]
    {
        match crate::embed::Embedder::load() {
            Ok(embedder) => return Ok(Memory8::with_embedder(store, Arc::new(embedder))),
            Err(e) => eprintln!("mem8: semantic search unavailable, using keyword search: {e}"),
        }
    }

    Ok(Memory8::new(store))
}

/// Backfill embeddings for memories that have none. Returns how many were
/// embedded.
pub async fn reindex() -> Result<usize> {
    build_service().await?.reindex(64).await
}

/// Write every memory to a markdown file. Returns the number exported.
pub async fn export(path: &Path) -> Result<usize> {
    let service = Memory8::new(open_from_env().await?);
    let memories = service.all().await?;
    std::fs::write(path, markdown::to_markdown(&memories)).map_err(io_err(path))?;
    Ok(memories.len())
}

/// Read memories from a markdown file into the store. Returns the number
/// imported. Existing memories are left untouched; imports always create new
/// rows.
pub async fn import(path: &Path) -> Result<usize> {
    let text = std::fs::read_to_string(path).map_err(io_err(path))?;
    let incoming = markdown::from_markdown(&text)?;

    // A file with content but no recognisable sections is almost always a
    // mistake — the wrong path, or a file whose headings are malformed. Say so
    // rather than reporting a successful import of nothing.
    if incoming.is_empty() && !text.trim().is_empty() {
        eprintln!(
            "warning: {} contains no memories. Each one needs a '## <uuid>' \
             heading followed by 'project' and 'kind' lines.",
            path.display()
        );
    }

    let store = open_from_env().await?;
    let service = Arc::new(Memory8::new(store));

    let mut count = 0;
    for m in incoming {
        service
            .add(&m.content, m.kind, m.tags, Some(m.project))
            .await?;
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    // `export` and `import` resolve their backend from `MEM8_DB`, so tests that
    // call them belong in `tests/cli_roundtrip.rs`, which owns its own process.
    // These cover only the file-error path, which never reaches a store.

    #[tokio::test]
    async fn importing_a_missing_file_names_the_path() {
        let missing = std::env::temp_dir().join(format!("mem8-absent-{}.md", uuid::Uuid::new_v4()));

        let message = match import(&missing).await {
            Ok(_) => panic!("importing a nonexistent file must fail"),
            Err(e) => e.to_string(),
        };

        assert!(
            message.contains(&missing.display().to_string()),
            "error should name the offending path, got: {message}"
        );
        assert!(
            !message.contains("store error"),
            "a missing file is not a store failure, got: {message}"
        );
    }
}

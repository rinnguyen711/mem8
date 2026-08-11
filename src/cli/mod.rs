pub mod markdown;

use crate::core::Memory8;
use crate::error::{Mem8Error, Result};
use crate::store::open_from_env;
use std::path::Path;
use std::sync::Arc;

fn io_err<E: std::fmt::Display>(e: E) -> Mem8Error {
    Mem8Error::Store(e.to_string())
}

/// Write every memory to a markdown file. Returns the number exported.
pub async fn export(path: &Path) -> Result<usize> {
    let service = Memory8::new(open_from_env().await?);
    let memories = service.all().await?;
    std::fs::write(path, markdown::to_markdown(&memories)).map_err(io_err)?;
    Ok(memories.len())
}

/// Read memories from a markdown file into the store. Returns the number
/// imported. Existing memories are left untouched; imports always create new
/// rows.
pub async fn import(path: &Path) -> Result<usize> {
    let text = std::fs::read_to_string(path).map_err(io_err)?;
    let incoming = markdown::from_markdown(&text)?;

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

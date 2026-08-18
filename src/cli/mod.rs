pub mod markdown;

use crate::core::Memory8;
use crate::error::{Mem8Error, Result};
use crate::store::{open_from_env, Store};
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::path::Path;
use uuid::Uuid;

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
            Ok(embedder) => {
                return Ok(Memory8::with_embedder(store, std::sync::Arc::new(embedder)))
            }
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
///
/// Because every row is new, a `superseded_by` read from the file names an id
/// that does not exist here. Those pointers are remapped onto the rows this
/// import created, in a second pass. A memory whose successor is not in the file
/// stays invalid with no successor recorded: the fact is still known to be dead,
/// and resurrecting it would be worse than losing the pointer.
///
/// Writes go to the store directly rather than through `core::add`, which would
/// apply duplicate detection -- importing a file twice would then merge the
/// second batch's live memories into the first batch's rows, leaving the second
/// batch's dead memories pointing across at the first batch's successors, and
/// any row the merge consumed dangling as a reference to an id that no longer
/// names a memory of that batch at all.
pub async fn import(path: &Path) -> Result<usize> {
    // Read the file before opening the store, so a missing or unreadable path
    // reports the IO error rather than a connection failure. The
    // `importing_a_missing_file_names_the_path` test below guards this: a
    // missing file is not a store failure, and saying so must not depend on the
    // database being reachable -- otherwise a typo'd path against a down
    // database costs a connection timeout and then names the wrong problem.
    let text = std::fs::read_to_string(path).map_err(io_err(path))?;
    import_text(open_from_env().await?.as_ref(), path, &text).await
}

/// `import` against an explicit store and already-read text, so the failure
/// paths can be tested.
///
/// Split out only for that: a test needs a store whose `supersede` fails on
/// demand, which is impossible to arrange through an environment variable.
/// `path` is carried along for error messages only.
pub async fn import_text(store: &dyn Store, path: &Path, text: &str) -> Result<usize> {
    let incoming = markdown::from_markdown(text)?;

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

    // Two passes. Load every memory first, recording old id -> new id, then
    // rewrite the successor pointers. A one-pass version cannot work: a
    // memory's successor may appear later in the file than it does.
    let mut mapping: HashMap<Uuid, Uuid> = HashMap::new();
    let mut pending: Vec<(Uuid, Option<Uuid>, DateTime<Utc>)> = Vec::new();
    let mut count = 0;

    for parsed in incoming {
        let created = store.add(parsed.new).await?;
        count += 1;

        if let Some(original) = parsed.original_id {
            mapping.insert(original, created.id);
        }
        if let Some(invalid_at) = parsed.invalid_at {
            pending.push((created.id, parsed.superseded_by, invalid_at));
        }
    }

    // Attempt every invalidation even if one fails, rather than aborting on the
    // first. Nothing spans the two passes transactionally, so an early return
    // would leave the remaining dead memories LIVE -- and because `supersede` is
    // write-once and import always creates fresh rows, re-running would not
    // repair them. Reporting every failure at the end keeps the damage visible
    // and bounded instead of silent and permanent.
    let mut failed: Vec<(Uuid, Mem8Error)> = Vec::new();
    // Captured before the loop consumes `pending`. This, not `count`, is the
    // denominator that means anything: `count` includes the successors, which
    // were never invalidation candidates.
    let candidates = pending.len();

    for (new_id, old_successor, invalid_at) in pending {
        let successor = old_successor.and_then(|s| mapping.get(&s).copied());

        // A successor outside this file: the memory is still known to be dead,
        // only its successor is unknown. This is exactly why `supersede` takes
        // `Option` -- dropping the invalidation instead would resurrect the
        // fact, which is the failure this round-tripping exists to prevent.
        //
        // Guarded on `is_some`: a file may legitimately carry `invalid_at` with
        // no `superseded_by` at all, and warning about a pointer that was never
        // there is noise.
        if successor.is_none() && old_successor.is_some() {
            eprintln!(
                "warning: imported memory {new_id} was superseded by a memory not \
                 present in this file; keeping it invalid with no successor recorded"
            );
        }

        if let Err(e) = store.supersede(new_id, successor, invalid_at).await {
            failed.push((new_id, e));
        }
    }

    // Loudly, rather than reporting a success that left live rows the file says
    // are dead. Naming them is what makes the damage actionable: nothing else
    // in the CLI can invalidate an existing row, so the user needs to know
    // which ones to deal with by hand.
    if !failed.is_empty() {
        let detail = failed
            .iter()
            .map(|(id, e)| format!("{id} ({e})"))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(Mem8Error::Store(format!(
            "imported {count} memories; {} of the {candidates} recorded as superseded \
             could not be marked, and are now live despite the file recording them as \
             replaced: {detail}. Re-importing will not repair this -- it creates a fresh \
             set of rows -- so these need handling by hand.",
            failed.len()
        )));
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

        // Point at a database that cannot be reached, so the guarantee is
        // pinned rather than incidental: reading the file has to come first, or
        // this fails on a connection timeout and names the wrong problem.
        //
        // Safe to set process-wide here, unlike in `tests/cli_roundtrip.rs`
        // where several tests share the variable: `import`, `export` and
        // `build_service` are the only readers of `MEM8_DB` in the library, and
        // this is the only test in this binary that calls any of them.
        std::env::set_var("MEM8_DB", "postgres://nobody@127.0.0.1:1/nope");

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

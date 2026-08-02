use blake3::Hash;
use turso::{Connection, Value, params_from_iter};
use uuid::Uuid;

use crate::error::{Error, Result};

pub(crate) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS paths (
    path BLOB NOT NULL,
    uuid BLOB NOT NULL,
    hash BLOB NOT NULL,
    PRIMARY KEY (path, uuid)
);
CREATE INDEX IF NOT EXISTS idx_paths_hash ON paths(hash);
"#;

#[derive(Debug)]
pub(crate) struct Entry {
    pub path: String,
    pub uuid: Uuid,
    pub hash: Hash,
}

// ---------------------------------------------------------------------------
// Value decoding
// ---------------------------------------------------------------------------

fn value_to_hash(v: &Value) -> Result<Hash> {
    let bytes = v
        .as_blob()
        .ok_or_else(|| Error::Other("hash column is not a blob".into()))?;
    let arr: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| Error::Other("hash has wrong length".into()))?;
    Ok(Hash::from(arr))
}

fn value_to_uuid(v: &Value) -> Result<Uuid> {
    let bytes = v
        .as_blob()
        .ok_or_else(|| Error::Other("uuid column is not a blob".into()))?;
    let arr: [u8; 16] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| Error::Other("uuid has wrong length".into()))?;
    Ok(Uuid::from_bytes(arr))
}

fn value_to_path(v: &Value) -> Result<String> {
    let bytes = v
        .as_blob()
        .ok_or_else(|| Error::Other("path column is not a blob".into()))?;
    String::from_utf8(bytes.clone())
        .map_err(|_| Error::Other("stored path is not valid UTF-8".into()))
}

// ---------------------------------------------------------------------------
// Write
// ---------------------------------------------------------------------------

/// Insert a shared (path, uuid) row for each path. Atomic: uses a single
/// multi-row INSERT. Duplicate (path, uuid) rows are ignored.
pub(crate) async fn insert_entries(
    conn: &Connection,
    paths: &[&str],
    hash: Hash,
    uuid: Uuid,
) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let placeholders = vec!["(?, ?, ?)"; paths.len()].join(", ");
    let sql = format!("INSERT OR IGNORE INTO paths(path, uuid, hash) VALUES {placeholders}");
    let mut params: Vec<Value> = Vec::with_capacity(paths.len() * 3);
    for path in paths {
        params.push(Value::Blob(path.as_bytes().to_vec()));
        params.push(Value::Blob(uuid.as_bytes().to_vec()));
        params.push(Value::Blob(hash.as_bytes().to_vec()));
    }
    conn.execute(sql, params_from_iter(params)).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------------

pub(crate) async fn get_latest(conn: &Connection, path: &str) -> Result<Option<(Hash, Uuid)>> {
    let mut rows = conn
        .query(
            "SELECT uuid, hash FROM paths WHERE path = ? ORDER BY uuid DESC LIMIT 1",
            params_from_iter(vec![Value::Blob(path.as_bytes().to_vec())]),
        )
        .await?;
    let Some(row) = rows.next().await? else {
        return Ok(None);
    };
    let hash = value_to_hash(&row.get_value(1)?)?;
    let uuid = value_to_uuid(&row.get_value(0)?)?;
    Ok(Some((hash, uuid)))
}

/// Versions ordered newest-first.
pub(crate) async fn list_versions(conn: &Connection, path: &str) -> Result<Vec<(Hash, Uuid)>> {
    let mut rows = conn
        .query(
            "SELECT uuid, hash FROM paths WHERE path = ? ORDER BY uuid DESC",
            params_from_iter(vec![Value::Blob(path.as_bytes().to_vec())]),
        )
        .await?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await? {
        let hash = value_to_hash(&row.get_value(1)?)?;
        let uuid = value_to_uuid(&row.get_value(0)?)?;
        out.push((hash, uuid));
    }
    Ok(out)
}

pub(crate) async fn list_prefix_entries(conn: &Connection, prefix: &str) -> Result<Vec<Entry>> {
    let (start, end) = prefix_range(prefix);
    let mut rows = conn
        .query(
            "SELECT path, uuid, hash FROM paths WHERE path >= ? AND path < ? ORDER BY path, uuid",
            params_from_iter(vec![Value::Blob(start), Value::Blob(end)]),
        )
        .await?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await? {
        let path = value_to_path(&row.get_value(0)?)?;
        let uuid = value_to_uuid(&row.get_value(1)?)?;
        let hash = value_to_hash(&row.get_value(2)?)?;
        out.push(Entry { path, uuid, hash });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------

/// Delete the oldest row matching (path, hash). Returns whether a row was
/// removed.
pub(crate) async fn delete_version(conn: &Connection, path: &str, hash: Hash) -> Result<bool> {
    let sql = "DELETE FROM paths WHERE rowid = \
        (SELECT rowid FROM paths WHERE path = ? AND hash = ? ORDER BY uuid ASC LIMIT 1)";
    let params = params_from_iter(vec![
        Value::Blob(path.as_bytes().to_vec()),
        Value::Blob(hash.as_bytes().to_vec()),
    ]);
    let removed = conn.execute(sql, params).await?;
    Ok(removed > 0)
}

/// Delete every version of path, returning the distinct hashes removed.
pub(crate) async fn delete_all_versions(conn: &Connection, path: &str) -> Result<Vec<Hash>> {
    let mut rows = conn
        .query(
            "SELECT DISTINCT hash FROM paths WHERE path = ?",
            params_from_iter(vec![Value::Blob(path.as_bytes().to_vec())]),
        )
        .await?;
    let mut hashes = Vec::new();
    while let Some(row) = rows.next().await? {
        hashes.push(value_to_hash(&row.get_value(0)?)?);
    }
    conn.execute(
        "DELETE FROM paths WHERE path = ?",
        params_from_iter(vec![Value::Blob(path.as_bytes().to_vec())]),
    )
    .await?;
    Ok(hashes)
}

// ---------------------------------------------------------------------------
// Reference counting (computed)
// ---------------------------------------------------------------------------

pub(crate) async fn ref_count(conn: &Connection, hash: Hash) -> Result<u32> {
    let mut rows = conn
        .query(
            "SELECT COUNT(*) FROM paths WHERE hash = ?",
            params_from_iter(vec![Value::Blob(hash.as_bytes().to_vec())]),
        )
        .await?;
    let row = rows
        .next()
        .await?
        .ok_or_else(|| Error::Other("COUNT query returned no rows".into()))?;
    let n = row.get_value(0)?;
    n.as_integer()
        .copied()
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| Error::Other("count is not a non-negative integer".into()))
}

// ---------------------------------------------------------------------------
// Full scan
// ---------------------------------------------------------------------------

pub(crate) async fn all_entries(conn: &Connection) -> Result<Vec<Entry>> {
    let mut rows = conn
        .query("SELECT path, uuid, hash FROM paths ORDER BY path, uuid", ())
        .await?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await? {
        let path = value_to_path(&row.get_value(0)?)?;
        let uuid = value_to_uuid(&row.get_value(1)?)?;
        let hash = value_to_hash(&row.get_value(2)?)?;
        out.push(Entry { path, uuid, hash });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Range helpers
// ---------------------------------------------------------------------------

/// Byte-exact half-open range [prefix, prefix with last byte incremented).
/// Paths are stored as BLOB so comparisons are bytewise, avoiding UTF-8
/// boundary issues on the incremented end byte.
fn prefix_range(prefix: &str) -> (Vec<u8>, Vec<u8>) {
    let start = prefix.as_bytes().to_vec();
    let mut end = prefix.as_bytes().to_vec();
    if let Some(last) = end.last_mut() {
        *last = last.wrapping_add(1);
    } else {
        end.push(0xff);
    }
    (start, end)
}

// Copyright 2026 Signal Messenger, LLC / my-sd Rust Backend
// SPDX-License-Identifier: AGPL-3.0-only

//! Native Rust SQLCipher backend replacing `ts/sql/main.main.ts`,
//! `ts/sql/mainWorker.node.ts`, `ts/sql/Server.node.ts`, and `app/sql_channel.main.ts`.
//!
//! Opens Signal Desktop's existing `~/.config/Signal/sql/db.sqlite` using
//! `PRAGMA key = "x'<hex>'"` and applies Raspberry Pi 4B microSD-friendly WAL PRAGMAs.

use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;
use rusqlite::{params, types::ValueRef, Connection, OpenFlags};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{info, warn};

/// Thread-safe native SQLCipher manager with write-pause support.
pub struct SqlEngine {
    db_path: PathBuf,
    writer: Mutex<Option<Connection>>,
    reader: Mutex<Option<Connection>>,
    writes_paused: AtomicBool,
}

impl SqlEngine {
    /// Opens (or creates) `<user_data_dir>/sql/db.sqlite` encrypted with `hex_key`.
    pub fn open(user_data_dir: &Path, hex_key: &str) -> Result<Self> {
        if hex_key.is_empty() || !hex_key.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(anyhow!("Invalid SQLCipher hex key"));
        }

        let sql_dir = user_data_dir.join("sql");
        fs::create_dir_all(&sql_dir)
            .with_context(|| format!("Failed to create {}", sql_dir.display()))?;
        let db_path = sql_dir.join("db.sqlite");

        let writer_conn = open_and_configure_sqlcipher(&db_path, hex_key, false)?;
        ensure_core_tables(&writer_conn)?;

        let reader_conn = open_and_configure_sqlcipher(&db_path, hex_key, true)?;

        info!("SQLCipher database opened at {}", db_path.display());

        Ok(Self {
            db_path,
            writer: Mutex::new(Some(writer_conn)),
            reader: Mutex::new(Some(reader_conn)),
            writes_paused: AtomicBool::new(false),
        })
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    pub fn pause_write_access(&self) {
        self.writes_paused.store(true, Ordering::SeqCst);
        info!("SQL write access paused");
    }

    pub fn resume_write_access(&self) {
        self.writes_paused.store(false, Ordering::SeqCst);
        info!("SQL write access resumed");
    }

    pub fn is_writes_paused(&self) -> bool {
        self.writes_paused.load(Ordering::SeqCst)
    }

    /// Runs a WAL checkpoint (`PRAGMA wal_checkpoint(TRUNCATE)`).
    pub fn wal_checkpoint(&self) -> Result<(i64, i64, i64)> {
        let guard = self.writer.lock();
        let conn = guard.as_ref().ok_or_else(|| anyhow!("Database is closed"))?;
        let row: (i64, i64, i64) =
            conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?;
        Ok(row)
    }

    /// Closes reader and writer connections cleanly (running `PRAGMA optimize`).
    pub fn close(&self) {
        if let Some(conn) = self.reader.lock().take() {
            let _ = conn.close();
        }
        if let Some(conn) = self.writer.lock().take() {
            let _ = conn.execute_batch("PRAGMA optimize;");
            let _ = conn.close();
        }
        info!("SQLCipher connections closed");
    }

    /// Closes connections and deletes `db.sqlite`, `-wal`, and `-shm`.
    pub fn remove_db(&self) -> Result<()> {
        self.close();
        for suffix in ["", "-wal", "-shm"] {
            let path = PathBuf::from(format!("{}{}", self.db_path.display(), suffix));
            if path.exists() {
                fs::remove_file(&path)?;
            }
        }
        info!("Removed SQLCipher database at {}", self.db_path.display());
        Ok(())
    }

    /// Dispatches a read-only SQL RPC method (matching `sql-channel:read`).
    pub fn dispatch_read(&self, method: &str, args: &[Value]) -> Result<Value> {
        let guard = self.reader.lock();
        let conn = guard.as_ref().ok_or_else(|| anyhow!("Database is closed"))?;

        match method {
            "getItemById" => {
                let id = arg_str(args, 0)?;
                query_json_column_by_id(conn, "SELECT json FROM items WHERE id = ?1", id)
            }
            "getAllItems" => {
                query_json_column_all(conn, "SELECT json FROM items ORDER BY id ASC")
            }
            "getConversationById" => {
                let id = arg_str(args, 0)?;
                query_json_column_by_id(conn, "SELECT json FROM conversations WHERE id = ?1", id)
            }
            "getConversationByE164" => {
                let e164 = arg_str(args, 0)?;
                query_json_column_by_id(conn, "SELECT json FROM conversations WHERE e164 = ?1", e164)
            }
            "getConversationByServiceId" => {
                let service_id = arg_str(args, 0)?;
                query_json_column_by_id(
                    conn,
                    "SELECT json FROM conversations WHERE serviceId = ?1",
                    service_id,
                )
            }
            "getAllConversations" => {
                query_json_column_all(conn, "SELECT json FROM conversations ORDER BY active_at DESC")
            }
            "getAllGroups" => query_json_column_all(
                conn,
                "SELECT json FROM conversations WHERE type = 'group' ORDER BY active_at DESC",
            ),
            "getMessageById" => {
                let id = arg_str(args, 0)?;
                query_json_column_by_id(conn, "SELECT json FROM messages WHERE id = ?1", id)
            }
            "getMessagesByConversation" => {
                let conv_id = arg_str(args, 0)?;
                let limit = args.get(1).and_then(|v| v.as_i64()).unwrap_or(50);
                let mut stmt = conn.prepare_cached(
                    "SELECT json FROM messages WHERE conversationId = ?1 ORDER BY received_at DESC LIMIT ?2",
                )?;
                let rows = stmt.query_map(params![conv_id, limit], |r| r.get::<_, String>(0))?;
                collect_json_rows(rows)
            }
            "getOlderMessagesByConversation" => {
                let conv_id = arg_str(args, 0)?;
                let before_ts = args
                    .get(1)
                    .and_then(|v| v.as_i64())
                    .ok_or_else(|| anyhow!("getOlderMessagesByConversation requires received_at timestamp"))?;
                let limit = args.get(2).and_then(|v| v.as_i64()).unwrap_or(50);
                let mut stmt = conn.prepare_cached(
                    "SELECT json FROM messages WHERE conversationId = ?1 AND received_at < ?2 ORDER BY received_at DESC LIMIT ?3",
                )?;
                let rows =
                    stmt.query_map(params![conv_id, before_ts, limit], |r| r.get::<_, String>(0))?;
                collect_json_rows(rows)
            }
            "getNewerMessagesByConversation" => {
                let conv_id = arg_str(args, 0)?;
                let after_ts = args
                    .get(1)
                    .and_then(|v| v.as_i64())
                    .ok_or_else(|| anyhow!("getNewerMessagesByConversation requires received_at timestamp"))?;
                let limit = args.get(2).and_then(|v| v.as_i64()).unwrap_or(50);
                let mut stmt = conn.prepare_cached(
                    "SELECT json FROM messages WHERE conversationId = ?1 AND received_at > ?2 ORDER BY received_at ASC LIMIT ?3",
                )?;
                let rows =
                    stmt.query_map(params![conv_id, after_ts, limit], |r| r.get::<_, String>(0))?;
                collect_json_rows(rows)
            }
            "getMessagesBySentAt" => {
                let sent_at = args
                    .first()
                    .and_then(|v| v.as_i64())
                    .ok_or_else(|| anyhow!("getMessagesBySentAt requires sent_at integer"))?;
                let mut stmt = conn.prepare_cached(
                    "SELECT json FROM messages WHERE sent_at = ?1 ORDER BY received_at DESC",
                )?;
                let rows = stmt.query_map(params![sent_at], |r| r.get::<_, String>(0))?;
                collect_json_rows(rows)
            }
            "getUnreadCount" => {
                let conv_id = args.first().and_then(|v| v.as_str());
                let count: i64 = if let Some(cid) = conv_id {
                    conn.query_row(
                        "SELECT COUNT(*) FROM messages WHERE conversationId = ?1 AND unread = 1",
                        params![cid],
                        |r| r.get(0),
                    )?
                } else {
                    conn.query_row(
                        "SELECT COUNT(*) FROM messages WHERE unread = 1",
                        [],
                        |r| r.get(0),
                    )?
                };
                Ok(json!(count))
            }
            "searchMessages" => {
                let query = arg_str(args, 0)?;
                let limit = args.get(1).and_then(|v| v.as_i64()).unwrap_or(50);
                let pattern = format!("%{query}%");
                let mut stmt = conn.prepare_cached(
                    "SELECT json FROM messages WHERE body LIKE ?1 ORDER BY received_at DESC LIMIT ?2",
                )?;
                let rows = stmt.query_map(params![pattern, limit], |r| r.get::<_, String>(0))?;
                collect_json_rows(rows)
            }
            "getMessageCount" => {
                let count: i64 =
                    conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?;
                Ok(json!(count))
            }
            "getUnprocessedCount" => {
                let count: i64 =
                    conn.query_row("SELECT COUNT(*) FROM unprocessed", [], |r| r.get(0))?;
                Ok(json!(count))
            }
            "getAllUnprocessed" => query_json_column_all(
                conn,
                "SELECT json FROM unprocessed ORDER BY timestamp ASC",
            ),
            "getUnprocessedById" => {
                let id = arg_str(args, 0)?;
                query_json_column_by_id(conn, "SELECT json FROM unprocessed WHERE id = ?1", id)
            }
            "getSessionById" => {
                let id = arg_str(args, 0)?;
                query_json_column_by_id(conn, "SELECT json FROM sessions WHERE id = ?1", id)
            }
            "getAllSessions" => query_json_column_all(conn, "SELECT json FROM sessions"),
            "getSessionsByNumber" => {
                let number = arg_str(args, 0)?;
                let mut stmt =
                    conn.prepare_cached("SELECT json FROM sessions WHERE number = ?1")?;
                let rows = stmt.query_map(params![number], |r| r.get::<_, String>(0))?;
                collect_json_rows(rows)
            }
            "getIdentityKeyById" => {
                let id = arg_str(args, 0)?;
                query_json_column_by_id(conn, "SELECT json FROM identityKeys WHERE id = ?1", id)
            }
            "getAllIdentityKeys" => query_json_column_all(conn, "SELECT json FROM identityKeys"),
            "getPreKeyById" => {
                let id = arg_str(args, 0)?;
                query_json_column_by_id(conn, "SELECT json FROM preKeys WHERE id = ?1", id)
            }
            "getAllPreKeys" => query_json_column_all(conn, "SELECT json FROM preKeys"),
            "getSignedPreKeyById" => {
                let id = arg_str(args, 0)?;
                query_json_column_by_id(conn, "SELECT json FROM signedPreKeys WHERE id = ?1", id)
            }
            "getAllSignedPreKeys" => query_json_column_all(conn, "SELECT json FROM signedPreKeys"),
            "queryRaw" => {
                let sql = arg_str(args, 0)?;
                let bind = args
                    .get(1)
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                query_raw_rows(conn, sql, &bind)
            }
            other => Err(anyhow!("Unsupported SQL read method: {other}")),
        }
    }

    /// Dispatches a read/write SQL RPC method (matching `sql-channel:write`).
    pub fn dispatch_write(&self, method: &str, args: &[Value]) -> Result<Value> {
        if self.is_writes_paused() && method != "close" {
            return Err(anyhow!(
                "SQL write access is currently paused (rejected {method})"
            ));
        }

        if method == "close" {
            self.close();
            return Ok(Value::Null);
        }

        if method == "walCheckpoint" {
            let (busy, log, checkpointed) = self.wal_checkpoint()?;
            return Ok(json!({ "busy": busy, "log": log, "checkpointed": checkpointed }));
        }

        let mut guard = self.writer.lock();
        let conn = guard.as_mut().ok_or_else(|| anyhow!("Database is closed"))?;

        match method {
            "createOrUpdateItem" => {
                let item = args
                    .first()
                    .ok_or_else(|| anyhow!("createOrUpdateItem requires item argument"))?;
                let id = item
                    .get("id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("createOrUpdateItem: missing string id"))?;
                let serialized = serde_json::to_string(item)?;
                conn.execute(
                    "INSERT OR REPLACE INTO items (id, json) VALUES (?1, ?2)",
                    params![id, serialized],
                )?;
                Ok(Value::Null)
            }
            "removeItemById" => {
                let id = arg_str(args, 0)?;
                conn.execute("DELETE FROM items WHERE id = ?1", params![id])?;
                Ok(Value::Null)
            }
            "saveConversation" => {
                let conv = args
                    .first()
                    .ok_or_else(|| anyhow!("saveConversation requires conversation argument"))?;
                let id = conv
                    .get("id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("saveConversation: missing string id"))?;
                let active_at = conv.get("active_at").and_then(|v| v.as_i64());
                let conv_type = conv
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("private");
                let e164 = conv.get("e164").and_then(|v| v.as_str());
                let service_id = conv.get("serviceId").and_then(|v| v.as_str());
                let serialized = serde_json::to_string(conv)?;
                conn.execute(
                    "INSERT OR REPLACE INTO conversations (id, json, active_at, type, e164, serviceId) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![id, serialized, active_at, conv_type, e164, service_id],
                )?;
                Ok(Value::Null)
            }
            "removeConversation" => {
                let id = arg_str(args, 0)?;
                conn.execute("DELETE FROM conversations WHERE id = ?1", params![id])?;
                Ok(Value::Null)
            }
            "saveMessage" => {
                let msg = args
                    .first()
                    .ok_or_else(|| anyhow!("saveMessage requires message argument"))?;
                let id = insert_message_row(conn, msg)?;
                Ok(json!(id))
            }
            "saveMessages" => {
                let list = args
                    .first()
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| anyhow!("saveMessages requires an array of messages"))?;
                let tx = conn.transaction()?;
                let mut ids = Vec::with_capacity(list.len());
                for msg in list {
                    ids.push(Value::String(insert_message_row(&tx, msg)?));
                }
                tx.commit()?;
                Ok(Value::Array(ids))
            }
            "removeMessageById" => {
                let id = arg_str(args, 0)?;
                conn.execute("DELETE FROM messages WHERE id = ?1", params![id])?;
                Ok(Value::Null)
            }
            "removeMessagesByIds" => {
                let list = args
                    .first()
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| anyhow!("removeMessagesByIds requires an array of ids"))?;
                let tx = conn.transaction()?;
                for id_val in list {
                    if let Some(id) = id_val.as_str() {
                        tx.execute("DELETE FROM messages WHERE id = ?1", params![id])?;
                    }
                }
                tx.commit()?;
                Ok(Value::Null)
            }
            "saveUnprocessed" => {
                let item = args
                    .first()
                    .ok_or_else(|| anyhow!("saveUnprocessed requires envelope argument"))?;
                let id = item
                    .get("id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("saveUnprocessed: missing string id"))?;
                let ts = item.get("timestamp").and_then(|v| v.as_i64()).unwrap_or(0);
                let serialized = serde_json::to_string(item)?;
                conn.execute(
                    "INSERT OR REPLACE INTO unprocessed (id, timestamp, json) VALUES (?1, ?2, ?3)",
                    params![id, ts, serialized],
                )?;
                Ok(Value::Null)
            }
            "removeUnprocessed" => {
                let id = arg_str(args, 0)?;
                conn.execute("DELETE FROM unprocessed WHERE id = ?1", params![id])?;
                Ok(Value::Null)
            }
            "removeAllUnprocessed" => {
                conn.execute("DELETE FROM unprocessed", [])?;
                Ok(Value::Null)
            }
            "saveSession" => {
                let id = arg_str(args, 0)?;
                let data = args
                    .get(1)
                    .ok_or_else(|| anyhow!("saveSession requires data argument"))?;
                let number = data
                    .get("number")
                    .and_then(|v| v.as_str())
                    .or_else(|| id.split('.').next());
                let serialized = serde_json::to_string(data)?;
                conn.execute(
                    "INSERT OR REPLACE INTO sessions (id, number, json) VALUES (?1, ?2, ?3)",
                    params![id, number, serialized],
                )?;
                Ok(Value::Null)
            }
            "removeSessionById" => {
                let id = arg_str(args, 0)?;
                conn.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
                Ok(Value::Null)
            }
            "removeAllSessionsByNumber" => {
                let number = arg_str(args, 0)?;
                conn.execute("DELETE FROM sessions WHERE number = ?1", params![number])?;
                Ok(Value::Null)
            }
            "saveIdentityKey" => {
                let id = arg_str(args, 0)?;
                let data = args
                    .get(1)
                    .ok_or_else(|| anyhow!("saveIdentityKey requires data argument"))?;
                let serialized = serde_json::to_string(data)?;
                conn.execute(
                    "INSERT OR REPLACE INTO identityKeys (id, json) VALUES (?1, ?2)",
                    params![id, serialized],
                )?;
                Ok(Value::Null)
            }
            "removeIdentityKeyById" => {
                let id = arg_str(args, 0)?;
                conn.execute("DELETE FROM identityKeys WHERE id = ?1", params![id])?;
                Ok(Value::Null)
            }
            "savePreKey" => {
                let id = arg_str(args, 0)?;
                let data = args
                    .get(1)
                    .ok_or_else(|| anyhow!("savePreKey requires data argument"))?;
                let serialized = serde_json::to_string(data)?;
                conn.execute(
                    "INSERT OR REPLACE INTO preKeys (id, json) VALUES (?1, ?2)",
                    params![id, serialized],
                )?;
                Ok(Value::Null)
            }
            "removePreKeyById" => {
                let id = arg_str(args, 0)?;
                conn.execute("DELETE FROM preKeys WHERE id = ?1", params![id])?;
                Ok(Value::Null)
            }
            "saveSignedPreKey" => {
                let id = arg_str(args, 0)?;
                let data = args
                    .get(1)
                    .ok_or_else(|| anyhow!("saveSignedPreKey requires data argument"))?;
                let serialized = serde_json::to_string(data)?;
                conn.execute(
                    "INSERT OR REPLACE INTO signedPreKeys (id, json) VALUES (?1, ?2)",
                    params![id, serialized],
                )?;
                Ok(Value::Null)
            }
            "execRaw" => {
                let sql = arg_str(args, 0)?;
                let bind = args
                    .get(1)
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let rusqlite_params: Vec<rusqlite::types::Value> =
                    bind.iter().map(json_to_sql_value).collect();
                let changed = conn.execute(sql, rusqlite::params_from_iter(rusqlite_params))?;
                Ok(json!({ "changes": changed }))
            }
            other => Err(anyhow!("Unsupported SQL write method: {other}")),
        }
    }
}

fn insert_message_row(conn: &Connection, msg: &Value) -> Result<String> {
    let id = msg
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("saveMessage: missing string id"))?;
    let conv_id = msg.get("conversationId").and_then(|v| v.as_str());
    let sent_at = msg.get("sent_at").and_then(|v| v.as_i64());
    let received_at = msg.get("received_at").and_then(|v| v.as_i64());
    let body = msg.get("body").and_then(|v| v.as_str());
    let unread = msg.get("unread").and_then(|v| v.as_i64()).unwrap_or(0);
    let serialized = serde_json::to_string(msg)?;
    conn.execute(
        "INSERT OR REPLACE INTO messages (id, json, conversationId, sent_at, received_at, body, unread) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![id, serialized, conv_id, sent_at, received_at, body, unread],
    )?;
    Ok(id.to_string())
}

fn collect_json_rows<F>(rows: rusqlite::MappedRows<'_, F>) -> Result<Value>
where
    F: FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<String>,
{
    let mut out = Vec::new();
    for row in rows {
        let s = row?;
        out.push(serde_json::from_str::<Value>(&s).unwrap_or(Value::String(s)));
    }
    Ok(Value::Array(out))
}

/// Opens a SQLCipher connection, applies Signal Desktop's hex key, attempts
/// SQLCipher v3 -> v4 migration if needed, and sets Raspberry Pi 4B WAL PRAGMAs.
fn open_and_configure_sqlcipher(
    db_path: &Path,
    hex_key: &str,
    read_only: bool,
) -> Result<Connection> {
    let flags = if read_only {
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
    };

    let open_once = |migrate_v3: bool| -> Result<Connection> {
        let conn = Connection::open_with_flags(db_path, flags)?;
        conn.pragma_update(None, "key", format!("x'{hex_key}'"))?;

        if migrate_v3 && !read_only {
            conn.pragma_update(None, "cipher_migrate", "")?;
        }

        // Verify the key works by reading sqlite_master
        let _: i64 = conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))?;

        apply_rpi4_pragmas(&conn, read_only)?;
        Ok(conn)
    };

    match open_once(false) {
        Ok(conn) => Ok(conn),
        Err(first_err) if !read_only => {
            warn!("Direct SQLCipher open failed ({first_err:#}); attempting cipher_migrate");
            open_once(true).with_context(|| {
                format!(
                    "Failed to open SQLCipher database at {} (check encryption key)",
                    db_path.display()
                )
            })
        }
        Err(err) => Err(err),
    }
}

/// Matches the Raspberry Pi 4B microSD optimizations from `ts/sql/Server.node.ts`.
fn apply_rpi4_pragmas(conn: &Connection, read_only: bool) -> Result<()> {
    conn.busy_timeout(std::time::Duration::from_secs(10))?;

    if !read_only {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        #[cfg(target_os = "linux")]
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        #[cfg(not(target_os = "linux"))]
        conn.pragma_update(None, "synchronous", "FULL")?;

        conn.pragma_update(None, "wal_autocheckpoint", 1000)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
    }

    #[cfg(target_os = "linux")]
    {
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        conn.pragma_update(None, "cache_size", -16000)?; // 16 MB page cache
        conn.pragma_update(None, "mmap_size", 67_108_864)?; // 64 MB mmap
    }

    Ok(())
}

/// Creates minimal core Signal Desktop tables if opening a brand-new database.
/// On an existing Signal Desktop database, `CREATE TABLE IF NOT EXISTS` is a no-op.
fn ensure_core_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS items (
            id STRING PRIMARY KEY,
            json TEXT
        );
        CREATE TABLE IF NOT EXISTS conversations (
            id STRING PRIMARY KEY,
            json TEXT,
            active_at INTEGER,
            type STRING,
            e164 STRING,
            serviceId STRING
        );
        CREATE INDEX IF NOT EXISTS conversations_e164 ON conversations (e164);
        CREATE INDEX IF NOT EXISTS conversations_serviceId ON conversations (serviceId);
        CREATE TABLE IF NOT EXISTS messages (
            id STRING PRIMARY KEY,
            json TEXT,
            conversationId STRING,
            sent_at INTEGER,
            received_at INTEGER,
            body TEXT,
            unread INTEGER DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS messages_conversation ON messages (
            conversationId,
            received_at
        );
        CREATE INDEX IF NOT EXISTS messages_unread ON messages (
            conversationId,
            unread
        );
        CREATE TABLE IF NOT EXISTS unprocessed (
            id STRING PRIMARY KEY,
            timestamp INTEGER,
            json TEXT
        );
        CREATE TABLE IF NOT EXISTS sessions (
            id STRING PRIMARY KEY,
            number STRING,
            json TEXT
        );
        CREATE INDEX IF NOT EXISTS sessions_number ON sessions (number);
        CREATE TABLE IF NOT EXISTS identityKeys (
            id STRING PRIMARY KEY,
            json TEXT
        );
        CREATE TABLE IF NOT EXISTS preKeys (
            id STRING PRIMARY KEY,
            json TEXT
        );
        CREATE TABLE IF NOT EXISTS signedPreKeys (
            id STRING PRIMARY KEY,
            json TEXT
        );
        "#,
    )?;
    Ok(())
}

fn arg_str(args: &[Value], idx: usize) -> Result<&str> {
    args.get(idx)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("Missing string argument at index {idx}"))
}

fn query_json_column_by_id(conn: &Connection, sql: &str, id: &str) -> Result<Value> {
    let mut stmt = conn.prepare_cached(sql)?;
    let mut rows = stmt.query(params![id])?;
    if let Some(row) = rows.next()? {
        let s: String = row.get(0)?;
        Ok(serde_json::from_str(&s).unwrap_or(Value::String(s)))
    } else {
        Ok(Value::Null)
    }
}

fn query_json_column_all(conn: &Connection, sql: &str) -> Result<Value> {
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    collect_json_rows(rows)
}

fn query_raw_rows(conn: &Connection, sql: &str, bind: &[Value]) -> Result<Value> {
    let mut stmt = conn.prepare(sql)?;
    let col_names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let sql_params: Vec<rusqlite::types::Value> = bind.iter().map(json_to_sql_value).collect();

    let mut rows = stmt.query(rusqlite::params_from_iter(sql_params))?;
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        let mut obj = Map::new();
        for (i, name) in col_names.iter().enumerate() {
            let val = match row.get_ref(i)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(n) => json!(n),
                ValueRef::Real(f) => json!(f),
                ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
                ValueRef::Blob(b) => Value::String(hex::encode(b)),
            };
            obj.insert(name.clone(), val);
        }
        result.push(Value::Object(obj));
    }
    Ok(Value::Array(result))
}

fn json_to_sql_value(v: &Value) -> rusqlite::types::Value {
    match v {
        Value::Null => rusqlite::types::Value::Null,
        Value::Bool(b) => rusqlite::types::Value::Integer(if *b { 1 } else { 0 }),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                rusqlite::types::Value::Integer(i)
            } else if let Some(f) = n.as_f64() {
                rusqlite::types::Value::Real(f)
            } else {
                rusqlite::types::Value::Null
            }
        }
        Value::String(s) => rusqlite::types::Value::Text(s.clone()),
        other => rusqlite::types::Value::Text(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const TEST_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const WRONG_KEY: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

    #[test]
    fn test_sqlcipher_encryption_and_crud() {
        let tmp = TempDir::new().unwrap();
        let engine = SqlEngine::open(tmp.path(), TEST_KEY).unwrap();

        engine
            .dispatch_write(
                "createOrUpdateItem",
                &[json!({ "id": "deviceName", "value": "rpi4-rust" })],
            )
            .unwrap();
        engine
            .dispatch_write(
                "saveMessage",
                &[json!({
                    "id": "msg-1",
                    "conversationId": "conv-1",
                    "body": "Hello from Rust SQLCipher!",
                    "sent_at": 1700000000000i64,
                    "received_at": 1700000000100i64,
                    "unread": 1
                })],
            )
            .unwrap();

        let item = engine
            .dispatch_read("getItemById", &[json!("deviceName")])
            .unwrap();
        assert_eq!(item["value"], "rpi4-rust");

        assert_eq!(engine.dispatch_read("getMessageCount", &[]).unwrap(), json!(1));
        assert_eq!(
            engine
                .dispatch_read("getUnreadCount", &[json!("conv-1")])
                .unwrap(),
            json!(1)
        );

        let search = engine
            .dispatch_read("searchMessages", &[json!("SQLCipher"), json!(10)])
            .unwrap();
        assert_eq!(search.as_array().unwrap().len(), 1);

        // Verify pause/resume write gate
        engine.pause_write_access();
        assert!(engine
            .dispatch_write("removeItemById", &[json!("deviceName")])
            .is_err());
        engine.resume_write_access();
        engine
            .dispatch_write("removeItemById", &[json!("deviceName")])
            .unwrap();

        engine.close();

        // Wrong key must fail
        assert!(SqlEngine::open(tmp.path(), WRONG_KEY).is_err());

        // Reopen with correct key preserves data
        let reopened = SqlEngine::open(tmp.path(), TEST_KEY).unwrap();
        assert_eq!(
            reopened.dispatch_read("getMessageCount", &[]).unwrap(),
            json!(1)
        );
    }

    #[test]
    fn test_extended_conversations_pagination_unprocessed_and_sessions() {
        let tmp = TempDir::new().unwrap();
        let engine = SqlEngine::open(tmp.path(), TEST_KEY).unwrap();

        // Conversations & groups
        engine
            .dispatch_write(
                "saveConversation",
                &[json!({
                    "id": "c-direct",
                    "type": "private",
                    "e164": "+15551234567",
                    "serviceId": "aci-123",
                    "active_at": 200
                })],
            )
            .unwrap();
        engine
            .dispatch_write(
                "saveConversation",
                &[json!({
                    "id": "c-group",
                    "type": "group",
                    "active_at": 300
                })],
            )
            .unwrap();

        assert_eq!(
            engine
                .dispatch_read("getConversationByE164", &[json!("+15551234567")])
                .unwrap()["id"],
            "c-direct"
        );
        assert_eq!(
            engine
                .dispatch_read("getConversationByServiceId", &[json!("aci-123")])
                .unwrap()["id"],
            "c-direct"
        );
        assert_eq!(
            engine.dispatch_read("getAllGroups", &[]).unwrap().as_array().unwrap().len(),
            1
        );

        // Batch messages + older/newer pagination
        engine
            .dispatch_write(
                "saveMessages",
                &[json!([
                    { "id": "m1", "conversationId": "c-direct", "body": "one", "sent_at": 1000, "received_at": 1000 },
                    { "id": "m2", "conversationId": "c-direct", "body": "two", "sent_at": 2000, "received_at": 2000 },
                    { "id": "m3", "conversationId": "c-direct", "body": "three", "sent_at": 3000, "received_at": 3000 }
                ])],
            )
            .unwrap();

        let older = engine
            .dispatch_read(
                "getOlderMessagesByConversation",
                &[json!("c-direct"), json!(2500), json!(10)],
            )
            .unwrap();
        assert_eq!(older.as_array().unwrap().len(), 2);

        let newer = engine
            .dispatch_read(
                "getNewerMessagesByConversation",
                &[json!("c-direct"), json!(1500), json!(10)],
            )
            .unwrap();
        assert_eq!(newer.as_array().unwrap().len(), 2);

        // Unprocessed envelope queue
        engine
            .dispatch_write(
                "saveUnprocessed",
                &[json!({ "id": "env-1", "timestamp": 5000, "envelope": "base64" })],
            )
            .unwrap();
        assert_eq!(
            engine.dispatch_read("getUnprocessedCount", &[]).unwrap(),
            json!(1)
        );
        engine
            .dispatch_write("removeAllUnprocessed", &[])
            .unwrap();
        assert_eq!(
            engine.dispatch_read("getUnprocessedCount", &[]).unwrap(),
            json!(0)
        );

        // Sessions by number
        engine
            .dispatch_write(
                "saveSession",
                &[json!("aci-123.1"), json!({ "number": "aci-123", "record": "abc" })],
            )
            .unwrap();
        engine
            .dispatch_write(
                "saveSession",
                &[json!("aci-123.2"), json!({ "number": "aci-123", "record": "def" })],
            )
            .unwrap();
        assert_eq!(
            engine
                .dispatch_read("getSessionsByNumber", &[json!("aci-123")])
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );
        engine
            .dispatch_write("removeAllSessionsByNumber", &[json!("aci-123")])
            .unwrap();
        assert_eq!(
            engine
                .dispatch_read("getSessionsByNumber", &[json!("aci-123")])
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }
}

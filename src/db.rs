#[cfg(feature = "sqlite")]
use rusqlite::{params, Connection, Result};
#[cfg(feature = "sqlite")]
use std::sync::{Arc, Mutex};
#[cfg(feature = "sqlite")]
use tokio::task;

// Thread-safe DB handle
#[cfg(feature = "sqlite")]
#[derive(Clone)]
pub struct Database {
    conn: Arc<Mutex<Connection>>,
}

#[cfg(feature = "sqlite")]
impl Database {
    pub fn init<P: AsRef<std::path::Path>>(path: P) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::setup_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Create an in-memory database for testing
    pub fn init_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::setup_schema(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn setup_schema(conn: &Connection) -> Result<()> {
        conn.execute(
            "CREATE TABLE IF NOT EXISTS documents (
                name TEXT PRIMARY KEY,
                data BLOB
            )",
            [],
        )?;
        Ok(())
    }

    pub async fn get_doc(&self, name: &str) -> Result<Option<Vec<u8>>> {
        let db = self.clone();
        let name = name.to_string();

        task::spawn_blocking(move || {
            let conn = db.conn.lock().unwrap();
            let mut stmt = conn.prepare("SELECT data FROM documents WHERE name = ?1")?;
            let mut rows = stmt.query(params![name])?;

            if let Some(row) = rows.next()? {
                Ok(Some(row.get(0)?))
            } else {
                Ok(None)
            }
        })
        .await
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?
    }

    pub async fn save_doc(&self, name: &str, data: Vec<u8>) -> Result<()> {
        let db = self.clone();
        let name = name.to_string();

        task::spawn_blocking(move || {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO documents (name, data) VALUES (?1, ?2)
                 ON CONFLICT(name) DO UPDATE SET data = ?2",
                params![name, data],
            )?;
            Ok(())
        })
        .await
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?
    }
}

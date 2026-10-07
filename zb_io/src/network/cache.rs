use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

pub struct ApiCache {
    conn: Connection,
}

impl std::fmt::Debug for ApiCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiCache").finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub body: String,
}

/// One formula of the bulk API file, as stored in the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry<'a> {
    pub name: String,
    /// The formula's JSON object, verbatim.
    pub body: &'a str,
    /// Other names that resolve to this formula.
    pub aliases: Vec<String>,
}

/// When and from what the index was last filled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexMeta {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// Unix seconds of the last successful fetch or revalidation.
    pub fetched_at: i64,
}

impl ApiCache {
    const SCHEMA_VERSION: u32 = 2;

    pub fn open(path: &Path) -> Result<Self, rusqlite::Error> {
        let conn = Connection::open(path)?;
        Self::migrate(&conn)?;
        Ok(Self { conn })
    }

    pub fn in_memory() -> Result<Self, rusqlite::Error> {
        let conn = Connection::open_in_memory()?;
        Self::migrate(&conn)?;
        Ok(Self { conn })
    }

    fn get_schema_version(conn: &Connection) -> Result<u32, rusqlite::Error> {
        let version: u32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        Ok(version)
    }

    fn set_schema_version(conn: &Connection, version: u32) -> Result<(), rusqlite::Error> {
        conn.execute(&format!("PRAGMA user_version = {}", version), [])?;
        Ok(())
    }

    fn migrate(conn: &Connection) -> Result<(), rusqlite::Error> {
        let current_version = Self::get_schema_version(conn)?;

        if current_version > Self::SCHEMA_VERSION {
            return Err(rusqlite::Error::InvalidQuery);
        }

        if current_version == Self::SCHEMA_VERSION {
            return Ok(());
        }

        for version in current_version..Self::SCHEMA_VERSION {
            let next_version = version + 1;
            Self::migrate_to_version(conn, next_version)?;
            Self::set_schema_version(conn, next_version)?;
        }

        Ok(())
    }

    fn migrate_to_version(conn: &Connection, version: u32) -> Result<(), rusqlite::Error> {
        match version {
            1 => Self::migrate_to_v1(conn),
            2 => Self::migrate_to_v2(conn),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    }

    /// The formula index: every formula of the bulk API file by name, the
    /// names that alias to them, and when the file was fetched.
    fn migrate_to_v2(conn: &Connection) -> Result<(), rusqlite::Error> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS formulas (
                name TEXT PRIMARY KEY,
                body TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS formula_aliases (
                alias TEXT PRIMARY KEY,
                name TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS index_meta (
                key TEXT PRIMARY KEY,
                value TEXT
            );",
        )
    }

    fn migrate_to_v1(conn: &Connection) -> Result<(), rusqlite::Error> {
        conn.execute(
            "CREATE TABLE IF NOT EXISTS api_cache (
                url TEXT PRIMARY KEY,
                etag TEXT,
                last_modified TEXT,
                body TEXT NOT NULL,
                cached_at INTEGER NOT NULL
            )",
            [],
        )?;
        Ok(())
    }

    pub fn get(&self, url: &str) -> Option<CacheEntry> {
        self.conn
            .query_row(
                "SELECT etag, last_modified, body FROM api_cache WHERE url = ?1",
                params![url],
                |row| {
                    Ok(CacheEntry {
                        etag: row.get(0)?,
                        last_modified: row.get(1)?,
                        body: row.get(2)?,
                    })
                },
            )
            .ok()
    }

    /// Clear all cached entries. Returns the number of entries removed.
    pub fn clear(&self) -> Result<usize, rusqlite::Error> {
        let removed = self.conn.execute("DELETE FROM api_cache", [])?;
        Ok(removed)
    }

    /// When the index was last filled, or `None` if it never was.
    pub fn index_meta(&self) -> Option<IndexMeta> {
        let get = |key: &str| -> Option<String> {
            self.conn
                .query_row(
                    "SELECT value FROM index_meta WHERE key = ?1",
                    params![key],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()
                .ok()
                .flatten()
                .flatten()
        };
        let fetched_at = get("fetched_at")?.parse().ok()?;
        Some(IndexMeta {
            etag: get("etag"),
            last_modified: get("last_modified"),
            fetched_at,
        })
    }

    /// Record that the index was confirmed current at `fetched_at`.
    pub fn touch_index(&self, fetched_at: i64) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "INSERT OR REPLACE INTO index_meta (key, value) VALUES ('fetched_at', ?1)",
            params![fetched_at.to_string()],
        )?;
        Ok(())
    }

    /// Replace the whole index in one transaction.
    pub fn replace_index<'a>(
        &self,
        entries: impl IntoIterator<Item = IndexEntry<'a>>,
        meta: &IndexMeta,
    ) -> Result<usize, rusqlite::Error> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM formulas", [])?;
        tx.execute("DELETE FROM formula_aliases", [])?;
        let mut count = 0;
        {
            let mut insert_formula =
                tx.prepare("INSERT OR REPLACE INTO formulas (name, body) VALUES (?1, ?2)")?;
            let mut insert_alias =
                tx.prepare("INSERT OR IGNORE INTO formula_aliases (alias, name) VALUES (?1, ?2)")?;
            for entry in entries {
                insert_formula.execute(params![entry.name, entry.body])?;
                for alias in &entry.aliases {
                    insert_alias.execute(params![alias, entry.name])?;
                }
                count += 1;
            }
        }
        for (key, value) in [
            ("etag", meta.etag.clone()),
            ("last_modified", meta.last_modified.clone()),
            ("fetched_at", Some(meta.fetched_at.to_string())),
        ] {
            tx.execute(
                "INSERT OR REPLACE INTO index_meta (key, value) VALUES (?1, ?2)",
                params![key, value],
            )?;
        }
        tx.commit()?;
        Ok(count)
    }

    pub fn index_formula_count(&self) -> Result<usize, rusqlite::Error> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM formulas", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    /// The JSON of the formula called `name`, if the index has it.
    pub fn index_formula(&self, name: &str) -> Option<String> {
        self.conn
            .query_row(
                "SELECT body FROM formulas WHERE name = ?1",
                params![name],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten()
    }

    /// The formula an alias or old name points at.
    pub fn index_alias(&self, alias: &str) -> Option<String> {
        self.conn
            .query_row(
                "SELECT name FROM formula_aliases WHERE alias = ?1",
                params![alias],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten()
    }

    /// Every name the index knows: formulas first, then aliases.
    pub fn index_names(&self) -> Result<Vec<String>, rusqlite::Error> {
        let mut names = Vec::new();
        for sql in [
            "SELECT name FROM formulas ORDER BY name",
            "SELECT alias FROM formula_aliases ORDER BY alias",
        ] {
            let mut stmt = self.conn.prepare(sql)?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            for name in rows {
                names.push(name?);
            }
        }
        Ok(names)
    }

    pub fn put(&self, url: &str, entry: &CacheEntry) -> Result<(), rusqlite::Error> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        self.conn.execute(
            "INSERT OR REPLACE INTO api_cache (url, etag, last_modified, body, cached_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![url, entry.etag, entry.last_modified, entry.body, now],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_and_retrieves_cache_entry() {
        let cache = ApiCache::in_memory().unwrap();

        let entry = CacheEntry {
            etag: Some("abc123".to_string()),
            last_modified: None,
            body: r#"{"name":"foo"}"#.to_string(),
        };

        cache.put("https://example.com/foo.json", &entry).unwrap();
        let retrieved = cache.get("https://example.com/foo.json").unwrap();

        assert_eq!(retrieved.etag, Some("abc123".to_string()));
        assert_eq!(retrieved.body, r#"{"name":"foo"}"#);
    }

    #[test]
    fn returns_none_for_missing_entry() {
        let cache = ApiCache::in_memory().unwrap();
        assert!(cache.get("https://example.com/nonexistent.json").is_none());
    }

    #[test]
    fn clear_removes_all_entries() {
        let cache = ApiCache::in_memory().unwrap();
        let entry = CacheEntry {
            etag: None,
            last_modified: None,
            body: "{}".to_string(),
        };
        cache.put("https://example.com/a.json", &entry).unwrap();
        cache.put("https://example.com/b.json", &entry).unwrap();

        let removed = cache.clear().unwrap();
        assert_eq!(removed, 2);
        assert!(cache.get("https://example.com/a.json").is_none());
        assert!(cache.get("https://example.com/b.json").is_none());
    }

    #[test]
    fn clear_on_empty_cache_returns_zero() {
        let cache = ApiCache::in_memory().unwrap();
        assert_eq!(cache.clear().unwrap(), 0);
    }

    #[test]
    fn new_database_starts_at_the_current_version() {
        let cache = ApiCache::in_memory().expect("failed to create cache");
        let version = ApiCache::get_schema_version(&cache.conn).expect("failed to get version");
        assert_eq!(version, ApiCache::SCHEMA_VERSION);
    }

    #[test]
    fn index_round_trips_formulas_aliases_and_meta() {
        let cache = ApiCache::in_memory().unwrap();
        assert!(cache.index_meta().is_none());
        assert!(cache.index_formula("jq").is_none());

        let meta = IndexMeta {
            etag: Some("\"v1\"".into()),
            last_modified: None,
            fetched_at: 1000,
        };
        let count = cache
            .replace_index(
                [
                    IndexEntry {
                        name: "pkgconf".into(),
                        body: r#"{"name":"pkgconf"}"#,
                        aliases: vec!["pkg-config".into(), "pkgconfig".into()],
                    },
                    IndexEntry {
                        name: "jq".into(),
                        body: r#"{"name":"jq"}"#,
                        aliases: vec![],
                    },
                ],
                &meta,
            )
            .unwrap();

        assert_eq!(count, 2);
        assert_eq!(cache.index_formula_count().unwrap(), 2);
        assert_eq!(cache.index_meta(), Some(meta));
        assert_eq!(
            cache.index_formula("jq").as_deref(),
            Some(r#"{"name":"jq"}"#)
        );
        assert_eq!(cache.index_alias("pkg-config").as_deref(), Some("pkgconf"));
        assert_eq!(cache.index_alias("pkgconf"), None);
        assert_eq!(
            cache.index_names().unwrap(),
            ["jq", "pkgconf", "pkg-config", "pkgconfig"]
        );

        // A second fill replaces, never merges.
        cache
            .replace_index(
                [IndexEntry {
                    name: "jq".into(),
                    body: r#"{"name":"jq","v":2}"#,
                    aliases: vec![],
                }],
                &IndexMeta {
                    etag: None,
                    last_modified: Some("yesterday".into()),
                    fetched_at: 2000,
                },
            )
            .unwrap();
        assert!(cache.index_formula("pkgconf").is_none());
        assert!(cache.index_alias("pkg-config").is_none());
        assert_eq!(cache.index_meta().unwrap().etag, None);
        assert_eq!(cache.index_meta().unwrap().fetched_at, 2000);

        cache.touch_index(3000).unwrap();
        assert_eq!(cache.index_meta().unwrap().fetched_at, 3000);
    }

    #[test]
    fn v1_databases_gain_the_index_tables() {
        let conn = Connection::open_in_memory().unwrap();
        ApiCache::migrate_to_v1(&conn).unwrap();
        ApiCache::set_schema_version(&conn, 1).unwrap();
        conn.execute(
            "INSERT INTO api_cache VALUES ('rb:x', NULL, NULL, 'class X', 1)",
            [],
        )
        .unwrap();

        ApiCache::migrate(&conn).unwrap();

        let cache = ApiCache { conn };
        assert_eq!(cache.get("rb:x").unwrap().body, "class X");
        assert!(cache.index_meta().is_none());
        assert!(cache.index_names().unwrap().is_empty());
    }

    #[test]
    fn migration_is_idempotent() {
        let cache = ApiCache::in_memory().expect("failed to create cache");
        ApiCache::migrate(&cache.conn).expect("first migration failed");
        ApiCache::migrate(&cache.conn).expect("second migration failed");
        let version = ApiCache::get_schema_version(&cache.conn).expect("failed to get version");
        assert_eq!(version, ApiCache::SCHEMA_VERSION);
    }

    #[test]
    fn rejects_future_schema_version() {
        let conn = Connection::open_in_memory().expect("failed to open connection");
        ApiCache::set_schema_version(&conn, 999).expect("failed to set version");
        let err = ApiCache::migrate(&conn).unwrap_err();
        assert!(matches!(err, rusqlite::Error::InvalidQuery));
    }

    #[test]
    fn migration_preserves_existing_data() {
        let conn = Connection::open_in_memory().expect("failed to open connection");

        conn.execute(
            "CREATE TABLE api_cache (
                url TEXT PRIMARY KEY,
                etag TEXT,
                last_modified TEXT,
                body TEXT NOT NULL,
                cached_at INTEGER NOT NULL
            )",
            [],
        )
        .expect("failed to create old schema");

        conn.execute(
            "INSERT INTO api_cache VALUES ('https://example.com', 'abc', NULL, 'data', 123)",
            [],
        )
        .expect("failed to insert test data");

        ApiCache::migrate(&conn).expect("migration failed");

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM api_cache", [], |row| row.get(0))
            .expect("failed to count rows");
        assert_eq!(count, 1);

        let url: String = conn
            .query_row("SELECT url FROM api_cache", [], |row| row.get(0))
            .expect("failed to query data");
        assert_eq!(url, "https://example.com");
    }
}

Sure, let me walk through the implementation. The function you're really looking at handles the BM25 recall query.

```rust
pub fn recall_bm25(&self, project: &str, query: &str, limit: usize) -> Result<Vec<MemoryRecord>> {
    let mut stmt = self.conn.prepare(
        "SELECT m.id, m.project, m.kind, m.body, m.created_at
           FROM memories_fts f
           JOIN memories m ON m.id = f.rowid
          WHERE memories_fts MATCH ?1 AND m.project = ?2
       ORDER BY rank LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![query, project, limit as i64], |row| {
        Ok(MemoryRecord {
            id: row.get(0)?,
            project: row.get(1)?,
            kind: row.get(2)?,
            body: row.get(3)?,
            created_at: row.get(4)?,
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>().map_err(...)
}
```

The really important thing to notice is that `memories_fts MATCH ?1` is the FTS5 syntax for a BM25 query. The `ORDER BY rank` clause uses FTS5's built-in BM25 ranking automatically. You don't actually have to compute any scores yourself.

For a code reviewer, the things to call out are: parameter binding is correct (no SQL injection risk), the result iterator handles errors via `?`, and the lifetime of `stmt` is bound to `self.conn`. That last part is just a subtle Rust borrow-checker requirement.

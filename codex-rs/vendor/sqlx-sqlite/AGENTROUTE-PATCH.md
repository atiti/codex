This is the crates.io `sqlx-sqlite` 0.9.0 source, vendored to apply the
upstream compatibility change in SQLx commits 94aafe3 and 10462fa (#4402 and
#4502). The only functional change widens the `libsqlite3-sys` dependency
range to allow 0.39, enabling the SQLite security fix for RUSTSEC-2026-0459.
Remove this patch when a published SQLx release includes the same change.

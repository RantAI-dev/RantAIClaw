# SQLite store fixtures

Three database files written by the build before the rusqlite 0.40 move: `main` at `d269efaf`, rusqlite 0.37
with its bundled SQLite. They are read by `tests/sqlite_old_files_open.rs`, which checks that the current build
opens and reads them unchanged.

| File | Written through | Holds |
|---|---|---|
| `sessions.sqlite` | `SessionStore::open`, `new_session`, `append_message`, `fork_session` | 3 sessions (one is a fork), 4 messages |
| `brain.sqlite` | `SqliteMemory::new`, `Memory::store` | 3 memory entries, one scoped to a session, FTS table populated |
| `kb.sqlite` | `SqliteStore::open`, `create_document`, `store_chunks` | 1 document, 2 chunks with 4-dimension vectors |

The files carry the extension `.sqlite` because `.gitignore` and `.dockerignore` exclude `*.db`. The test copies
each one to the name the store expects (`sessions.db`, `memory/brain.db`, `kb.db`).

All content is placeholder text (`user_a`, `rantaiclaw_*`, quokka and lighthouse sentences). Each file was
checkpointed and set to `journal_mode=DELETE`, so no `-wal` or `-shm` file belongs with it.

The files are never edited in place. To regenerate them, build the previous release, write the stores with the
same calls as the table, then checkpoint and switch each file to `journal_mode=DELETE`.

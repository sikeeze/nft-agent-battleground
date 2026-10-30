-- 0001_init.sql — skema awal (Fase 1, blueprint §9)
--
-- ATURAN KEAMANAN (blueprint §3A/§3B): tidak ada kolom yang menyimpan rahasia.
-- - private key / mnemonic HANYA di keystore terenkripsi (Argon2id + XChaCha20),
--   passphrase tidak pernah ditulis ke disk.
-- - session_state (Google/X) disimpan sebagai blob terenkripsi di file vault,
--   bukan di SQLite — hanya metadata yang masuk tabel `accounts`.
-- - kolom `payload`/`detail` berisi JSON yang wajib bebas rahasia.

CREATE TABLE tasks (
    id              TEXT PRIMARY KEY,
    idempotency_key TEXT UNIQUE,          -- untuk mencegah dobel eksekusi saat resume/restart
    kind            TEXT NOT NULL,
    payload         TEXT NOT NULL,        -- JSON, bebas rahasia
    status          TEXT NOT NULL DEFAULT 'pending',
    scheduled_at    INTEGER,              -- unix detik; NULL untuk job event-triggered
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);
CREATE INDEX idx_tasks_status ON tasks (status);
CREATE INDEX idx_tasks_scheduled_at ON tasks (scheduled_at);

CREATE TABLE rules (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    kind       TEXT NOT NULL,             -- buy | sell | custom
    collection TEXT,
    params     TEXT NOT NULL,             -- JSON sesuai rules.toml
    enabled    INTEGER NOT NULL DEFAULT 1,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE intents (
    id         TEXT PRIMARY KEY,
    task_id    TEXT REFERENCES tasks (id),
    kind       TEXT NOT NULL,
    chain_id   INTEGER,
    contract   TEXT,
    params     TEXT NOT NULL,             -- JSON, bebas rahasia
    originator TEXT NOT NULL,             -- user | llm | skill | scheduler
    created_at INTEGER NOT NULL
);
CREATE INDEX idx_intents_task ON intents (task_id);

CREATE TABLE txs (
    id         TEXT PRIMARY KEY,
    intent_id  TEXT REFERENCES intents (id),
    chain_id   INTEGER NOT NULL,
    wallet_id  TEXT NOT NULL,
    nonce      INTEGER,
    tx_hash    TEXT,
    status     TEXT NOT NULL DEFAULT 'prepared', -- prepared | signed | sent | confirmed | failed
    payload    TEXT NOT NULL,             -- JSON, bebas rahasia (tidak ada signed payload Plaintext kunci)
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX idx_txs_status ON txs (status);
CREATE INDEX idx_txs_wallet ON txs (wallet_id);

CREATE TABLE audit (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    at      INTEGER NOT NULL,
    actor   TEXT NOT NULL,                -- user_id | cli | system
    action  TEXT NOT NULL,
    subject TEXT,                         -- contoh: alamat wallet disingkat (0xAb..12), BUKAN isi rahasia
    detail  TEXT                          -- JSON
);

CREATE TABLE accounts (
    id            TEXT PRIMARY KEY,
    kind          TEXT NOT NULL,          -- google | x | discord | custom
    profile_dir   TEXT NOT NULL,
    last_verified INTEGER,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
);

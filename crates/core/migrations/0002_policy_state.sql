-- 0002_policy_state.sql — state policy persisten (Fase 3, blueprint §F5/§9)
--
-- Supaya kill switch & spend harian TETAP AKTIF setelah restart (§6.2
-- "Proses mati / restart saat war"), state policy disimpan di SQLite.
--
-- ATURAN KEAMANAN: tidak ada rahasia di sini. `wallet` adalah alamat publik
-- (bukan rahasia, lihat WalletRef), `eth` adalah angka kumulatif spend.

CREATE TABLE policy_kill_switch (
    id         INTEGER PRIMARY KEY,   -- selalu 1 ( singleton )
    active     INTEGER NOT NULL,      -- 1 = aktif, 0 = mati
    reason     TEXT,
    updated_at INTEGER NOT NULL
);

CREATE TABLE policy_spend (
    day        TEXT NOT NULL,         -- YYYY-MM-DD UTC
    wallet     TEXT NOT NULL,         -- alamat publik (bukan rahasia)
    eth        REAL NOT NULL,         -- kumulatif spend hari itu
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (day, wallet)
);

-- Riwayat pengukuran latensi RPC (§8.3 measure_latency, acceptance Fase 3).
CREATE TABLE latency_stats (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    endpoint TEXT NOT NULL,
    samples  INTEGER,
    min_ms   INTEGER,
    p50_ms   INTEGER,
    p95_ms   INTEGER,
    max_ms   INTEGER,
    at       INTEGER NOT NULL
);

# NEXT_AGENT.md — Instruksi untuk Agent Berikutnya (nft-agent)

> Kamu meneruskan proyek `nft-agent` sesuai `NFT_AGENT_BLUEPRINT_TELEGRAM.md`.
> Baca file blueprint itu **penuh** dulu — terutama §3 (keamanan), §6 (reliability),
> §9 (jadwal fase), dan §11 (prompt awal + aturan keras).

## Status proyek (per 2026-10-30)

### ✅ Fase 1 — Fondasi: SELESAI

- Cargo workspace di root, 20 member crate (`crates/*`), profil rilis
  `lto="fat"`, `codegen-units=1`, `opt-level=3` (mimalloc di binary
  `crates/agent`), CI GitHub Actions (fmt + clippy + test).
- `crates/core`: `Intent` (+IntentKind/Originator), `Task` (+TaskKind/TaskStatus
  dengan idempotency key), `WalletRef` (dengan `short_address()`), `Event`
  (+EventKind/Priority), `Secret<T>` (zeroize saat drop, Debug/Display
  `[REDACTED]`, tidak ada Clone/Copy, tidak ada Serialize).
- Config loader (`crates/core/src/config.rs`): `agent.toml`, `policy.toml`,
  `models.toml` — semua `deny_unknown_fields` + validasi cross-field fail-fast
  (pesan menyebut file & field). Contoh config ada di `config/`.
- Storage (`crates/core/src/storage.rs`): SQLite WAL + foreign keys +
  `create_if_missing`, migrasi `na`-style di `crates/core/migrations/0001_init.sql`
  (tabel `tasks`, `rules`, `intents`, `txs`, `audit`, `accounts`; tanpa kolom rahasia).

### ✅ Fase 2 — Keystore, Signer, Policy: SELESAI, semua Acceptance lulus

- **`crates/keystore`** (11 test): format biner `magic(7)|version(1)|salt(32)|
  nonce(24)|ciphertext`; **Argon2id** (m=64MiB, t=3, p=1) KDF +
  **XChaCha20-Poly1305** AEAD — bukan stub lagi. Passphrase salah →
  `WrongPassphrase` (AEAD tag gagal). Private key hanya ada di dalam ciphertext;
  test memastikan hex key tidak muncul di file maupun `Debug`. Save pakai atomic
  write (tmp + rename). `OpenWallet` menyimpan `Secret<Vec<u8>>` (32 byte) dan
  menyediakan `.signer()` → `PrivateKeySigner` alloy.
- **`crates/signer`** (6 test): `ApprovedTx` dengan konstruktor `pub(crate)`
  (hanya policy yang bisa membuatnya) — **tidak ada jalur sign tanpa policy**.
  Mendukung tx legacy (EIP-155) + EIP-1559, `personal_sign` (EIP-191), EIP-712
  typed data. Test: recover_signer dari envelope == address signer untuk kedua
  jenis tx; chain_id 0 ditolak; signature EIP-712 dapat diverifikasi.
- **`crates/policy`** (13 test): `PolicyEngine::policy_check` — pemeriksaan
  berurutan: kill switch (manual + auto saat rugi ≥ `auto_trigger_on_loss_eth`,
  persisten via `PolicyState`) → chain diizinkan → allowlist kontrak →
  `max_spend_per_tx_eth` → `max_spend_per_day_eth` (state harian) →
  `max_gas_gwei` → blokir `approve(MAX)`/`setApprovalForAll` (selector
  `0x095ea7b3`/`0xa22cb465`) → hook `HumanConfirmation` → hook `Simulator`
  (revert = tolak). `PolicyDecision::Approve(ApprovedTx)` adalah-satunya jalan
  ke signer. Trait `PolicyState`/`Simulator`/`HumanConfirmation` async
  (`async-trait`); implementasi SQLite/RPC/Telegram menyusul Fase 3/4; sekarang
  `InMemoryPolicyState`, `NoSimulator`, `AlwaysDenyConfirmation`.
- **Verifikasi terakhir sukses**: `cargo fmt --all && cargo clippy --all-targets
  -- -D warnings && cargo test --workspace` → **62 test hijau** (32 core + 11
  keystore + 13 policy + 6 signer), 0 gagal, 0 warning.
- Dep baru via `[workspace.dependencies]` root (aturan #9): `alloy` dengan
  features `signer-local`+`consensus`+`sol-types`+`eip712`+`k256`, `argon2`,
  `chacha20poly1305`, `hex`, `serde_bytes`, `async-trait` — semuanya dicantumkan
  dengan alasan pada baris komentar di root `Cargo.toml`.

### Catatan tooling lokal

- Windows (Git Bash/MSYS), Rust toolchain `stable` terpasang (rustc 1.99).
- Root proyek: `D:\AGENT WAR NFT`.

### Aturan keras (jangan dilanggar, dari blueprint §11)

1. Jangan pernah menyimpan/mencetak private key/mnemonic plaintext — hanya
   `Secret<T>` + keystore Argon2id+XChaCha20; phrase lewat chat hanya alur
   `/import` §3A (pesan dihapus, zeroize, tidak masuk log/SQLite).
2. Tidak ada jalur sign yang lewati Policy Engine; signer hanya menerima `ApprovedTx`.
3. Default **dry-run**; jangan mengirim tx asli tanpa flag eksplisit.
4. Jangan menyimpan password Google/X — hanya storage state terenkripsi (§3B).
5. Jangan menulis solver captcha / anti-detect bot.
6. Jangan taruh LLM di jalur eksekusi mint (hot path deterministik, §8.2).
7. WhatsApp: Cloud API resmi, satu arah, template, tanpa webhook/port publik, tidak blokir hot path.
8. Link mint wajib diverifikasi on-chain sebelum dieksekusi (§5).
9. Jangan menambah dependency baru tanpa via `[workspace.dependencies]`
   (root Cargo.toml) mencantumkan alasan di commit/NEXT_AGENT.md.
10. Setiap fase: implementasi → test sesuai Acceptance → `cargo fmt &&
    cargo clippy --all-targets -- -D warnings && cargo test --workspace`
    hijau → **lalu berhenti dan minta persetujuan user sebelum fase berikut.**

## Target berikutnya: Fase 3 (Hari 8-11) — Chain Layer

Urutan pengerjaan yang disarankan:

1. `crates/chain`:
   - RPC pool + WebSocket `newHeads`, failover, hedged requests (§8.2 poin 5-6).
   - Nonce manager (reservasi nonce, hindari nyangkut — blueprint §8.3
     `reserve_nonces`).
   - Gas EIP-1559 berbasis persentil; `measure_latency` (§8.3, §9 Fase 3).
   - Implementasi `Simulator` asli (`eth_call`/`eth_simulateV1`) yang menggantikan
     `NoSimulator` di `crates/policy`.
   - Implementasi `PolicyState` persisten SQLite (spend harian + kill switch
     tetap aktif setelah restart) menggantikan `InMemoryPolicyState`.
2. `crates/mint`:
   - Simulator/broadcaster (public + Flashbots Protect/bundle); fork test Anvil.
3. Acceptance Fase 3 (blueprint §9): transfer Sepolia full pipeline; revert
   terdeteksi di simulasi dan tx tidak terkirim; statistik latensi tersimpan.
4. Update bagian "Status proyek" di file ini setelah menyelesaikan fase.

## Checklist Acceptance tiap fase (ringkas, dari §9)

- F1 ✅ — `cargo test` hijau; config invalid ditolak dengan pesan jelas.
- F2 ✅ — batas policy & signer type-level; kunci tidak muncul di log.
- F3 — transfer Sepolia full pipeline; revert terdeteksi di simulasi; `measure_latency`.
- F4 — ack < 300 ms; non-whitelist diabaikan; `/stop` < 2 detik; phrase tidak muncul di log/db/balasan.
- F5 — fork Anvil: mint < 1 block; tidak dobel mint saat restart; dry-run tidak kirim tx.
- F6 — 10 link uji diproses benar; link scam tidak pernah jadi tx; preflight merah blokir war.
- F7 — tanpa password mentah; sign setApprovalForAll asing ditolak; SIWE domain salah ditolak.
- F8 — rule buy/list end-to-end testnet; PnL benar 3 skenario.
- F9 — ganti model tanpa ubah kode; prompt injection tidak menghasilkan intent berbahaya.
- F10 — job tetap jalan setelah restart; API localhost-only.
- F11 — event high/critical sampai WhatsApp tanpa rahasia; fallback Telegram teruji.
- F12 — target latensi §8.1 tercapai; 100 simulasi war tanpa kegagalan sisi-agent.
- F13 — audit trail, `cargo audit`, deploy VPS, uji bertahap 3 hari dry-run dulu.

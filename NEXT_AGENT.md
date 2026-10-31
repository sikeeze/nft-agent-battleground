# NEXT_AGENT.md — Instruksi untuk Agent Berikutnya (nft-agent)

> Kamu meneruskan proyek `nft-agent` sesuai `NFT_AGENT_BLUEPRINT_TELEGRAM.md`.
> Baca file blueprint itu **penuh** dulu — terutama §3 (keamanan), §6 (reliability),
> §9 (jadwal fase), dan §11 (prompt awal + aturan keras).

## Status proyek (per 2026-10-31)

### ✅ Fase 1 — Fondasi: SELESAI

- Cargo workspace di root, 20 member crate (`crates/*`), profil rilis
  `lto="fat"`, `codegen-units=1`, `opt-level=3` (mimalloc di binary
  `crates/agent`), CI GitHub Actions (fmt + clippy + test).
- `crates/core`: `Intent` (+IntentKind/Originator), `Task` (+TaskKind/TaskStatus
  dengan idempotency key), `WalletRef` (dengan `short_address()`), `Event`
  (+EventKind/Priority), `Secret<T>` (zeroize saat drop, Debug/Display
  `[REDACTED]`, tidak ada Clone/Copy, tidak ada Serialize).
- Config loader (`crates/core/src/config.rs`): `agent.toml`, `policy.toml`,
  `models.toml`, `chain.toml` — semua `deny_unknown_fields` + validasi
  cross-field fail-fast (pesan menyebut file & field). Contoh config ada
  di `config/`.
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
  Accessor tambahan Fase 3: `data_hex()`, `nonce()`.
- **`crates/policy`** (13 test): `PolicyEngine::policy_check` — pemeriksaan
  berurutan: kill switch (manual + auto saat rugi ≥ `auto_trigger_on_loss_eth`,
  persisten via `PolicyState`) → chain diizinkan → allowlist kontrak →
  `max_spend_per_tx_eth` → `max_spend_per_day_eth` (state harian) →
  `max_gas_gwei` → blokir `approve(MAX)`/`setApprovalForAll` (selector
  `0x095ea7b3`/`0xa22cb465`) → hook `HumanConfirmation` → hook `Simulator`
  (revert = tolak). `PolicyDecision::Approve(ApprovedTx)` adalah-satunya jalan
  ke signer. Trait `PolicyState`/`Simulator`/`HumanConfirmation` async
  (`async-trait`).
- Dep Fase 2 via `[workspace.dependencies]` root (aturan #9): `alloy` (features
  `signer-local`+`consensus`+`sol-types`+`eip712`+`k256`), `argon2`,
  `chacha20poly1305`, `hex`, `serde_bytes`, `async-trait`.

### ✅ Fase 3 — Chain Layer: SELESAI, semua Acceptance lulus

- **`crates/chain`** (29 unit test + 5 integration test live):
  - `pool.rs` — `RpcPool` multi-provider: satu `reqwest::Client` bersama
    (HTTP/2 keep-alive, connection pool), **hedged requests** (kirim ke
    cadangan setelah `hedge_delay_ms`, race, yang kalah dibatalkan — §8.2
    poin 6), **failover** (endpoint gagal → `Unhealthy` cooldown, bisa
    dipakai lagi setelahnya), `new_heads()` stream WebSocket `newHeads`
    (auto-reconnect, subscribe per endpoint, fallback polling
    `eth_getBlockByNumber` bila tak ada WS).
  - `nonce.rs` — `NonceManager` atomik per wallet: `sync_from`, `reserve`
    (monoton naik), `release` (hanya nonce terakhir), `reset` (§8.3
    `reserve_nonces`, §6.2 "Nonce bentrok / nyangkut").
  - `gas.rs` — `GasOracle` berbasis `eth_feeHistory` (persentil dari `rewards`
    nyata, bukan tebakan); mode `Normal`/`Fast`/`Aggressive` (p35/p60/p90);
    `max_fee_per_gas` dibatasi `max_gas_gwei` policy (§F5); fallback priority
    1 gwei bila node tak mengembalikan `rewards`; cache + `GasStats`.
  - `latency.rs` — `measure_latency` (§8.3): RTT per endpoint via
    `eth_chainId`, window sampel, p50/p95/min/max, `best_p95_ms()` untuk
    hitung `Tsend` (§7), `persist()` ke tabel `latency_stats`.
  - `simulator.rs` — `EthCallSimulator` mengimplementasikan trait
    `policy::Simulator` via `eth_call`: revert (JSON-RPC code 3 / pesan
    "revert") → `Err(reason)` → PolicyEngine menolak (§F5 "Simulasi wajib
    sebelum kirim"). Menggantikan `NoSimulator`.
  - `policy_state.rs` — `SqlitePolicyState` mengimplementasikan trait
    `policy::PolicyState` (kill switch persisten + spend harian akumulasi
    per hari per wallet) — menggantikan `InMemoryPolicyState`; skema di
    migrasi `0002_policy_state.sql` (tabel `policy_kill_switch`,
    `policy_spend`, `latency_stats`).
  - Integration test (`crates/chain/tests/integration.rs`) terhadap **RPC
    publik nyata Sepolia** (`ethereum-sepolia-rpc.publicnode.com`,
    `1rpc.io/sepolia`): chainId via hedged request, gas feeHistory live,
    measure_latency live, revert terdeteksi di simulasi (call ke MultiCall3
    selector asing), endpoint mati → error jelas bukan panic. Diskip
    otomatis bila offline (`NFT_SKIP_NETWORK_TESTS=1`) atau RPC tak
    terjangkau.
- **`crates/mint`** (5 test): `Broadcaster` + `submit_bundle` — kirim
  `SignedTx` ke public RPC (`eth_sendRawTransaction` via pool) atau **private
  relay Flashbots Protect** (§1 F1, §7). **Default `dry_run = true`** (aturan
  keras #3): `broadcast()` hanya mencatat tx hash, tidak mengirim. Envelope
  dienkode RLP + verifikasi signature via `recover_signer`.
- Config: `ChainConfig` (`chain.toml`) di `crates/core/src/config.rs` —
  `chain_id`, `rpc_urls`, `ws_urls`, `relay_url`, `hedge_delay_ms`,
  `request_timeout_ms`, `unhealthy_cooldown_ms`; validasi (rpc_urls tidak
  boleh kosong, ws harus `ws://`/`wss://`, dll). Contoh `config/chain.toml`
  (Base mainnet).
- Dep Fase 3 via `[workspace.dependencies]` (aturan #9): `reqwest`
  (json+rustls-tls+http2), `tokio-tungstenite` (rustls-tls-webpki-roots),
  `futures-util`, `alloy` tambah features `provider-http`+`provider-ws`+
  `rpc-types-eth`+`rpc-client`+`eips`+`serde`+`rlp`. Crate langsung: `tracing`,
  `dashmap` (chain).
- **Verifikasi terakhir sukses**: `cargo fmt --all && cargo clippy --all-targets
  -- -D warnings && cargo test --workspace` → **106 test hijau** (38 core + 11
  keystore + 13 policy + 6 signer + 29 chain unit + 5 chain integration +
  5 mint), 0 gagal, 0 warning.

### ✅ Fase 4 — Telegram Cepat: SELESAI, semua Acceptance lulus

- **`crates/telegram`** (54 unit test + 5 integration test):
  - `access.rs` — `AccessControl`: **whitelist `user_id`** ( pesan dari ID
    lain diabaikan **tanpa balasan**, §3 poin 2 ), **rate limit** per user
    per window, **lockout** setelah `max_failures_before_lockout`,
    `audit()` menulis ke tabel `audit` ( §3 poin 7 ).
  - `queue.rs` — `CommandQueue` ( `tokio::mpsc` ): handler hanya validasi +
    ack + enqueue ( §8.2 poin 1 hot/cold path ), ack terukur (
    `last_ack_ms` ), `take_reply_rx` untuk task pengirim balasan.
  - `confirm.rs` — `TelegramConfirmation` mengimplementasikan trait
    `policy::HumanConfirmation`: pesan + inline keyboard ( Konfirmasi/Tolak ),
    `answerCallbackQuery` instan + `editMessageText` ( §8.2 poin 8 ),
    timeout 120 dtk **default tolak** ( §F5 ). Menggantikan
    `AlwaysDenyConfirmation`.
  - `import.rs` — alur `/import` §3A penuh: `parse_phrase` ( hex 64 atau
    mnemonic 12/24 kata; **utamakan private key** ), `ImportSessions`
    ( timeout 2 menit, konfirmasi ), `verify_totp` ( TOTP SHA1 6-digit,
    §3 poin 5 ), `finalize_import` ( cek `burner_max_balance_eth` → tolak,
    cek `max_wallets`, simpan ke keystore Argon2id+XChaCha20 ),
    `zeroize_phrase` ( §3A poin 4 ). Mnemonic hanya menurunkan path
    `m/44'/60'/0'/0/0` via `MnemonicBuilder::try_from_phrase_first`.
    Fitur **mati default** ( `import_via_chat = false` ).
  - `notify.rs` — `Notifier` batched + prioritas ( §8.2 poin 8 ):
    `High`/`Critical` bypass batching ( kirim segera ), `Normal`/`Low`
    digabung dalam window, flush batch saat critical datang.
  - `bot.rs` — `BotState` + `handle_message` ( hot path: validasi +
    ack < 300 ms + enqueue + audit ), `process_command` ( `/status`,
    `/stop` ( trigger kill switch via `policy_state`, < 2 detik ),
    `/import`, `/newwallet`, `/wallets`, `/approve`, `/reject` ),
    `process_import_phrase` ( worker cold path: parse → finalize →
    balasan hanya alamat+saldo, **tidak pernah mengulang phrase** ),
    `spawn` ( dispatcher teloxide + `deleteMessage` sebelum proses lain,
    §3A poin 3 ).
  - `engine.rs` — `build_policy_engine` merakit `PolicyEngine` production:
    `SqlitePolicyState` + `EthCallSimulator` + `TelegramConfirmation`
    ( menggantikan `with_defaults` ).
  - Integration test ( `crates/telegram/tests/integration.rs` ): wiring
    engine dengan RPC Sepolia live ( revert terdeteksi ), kill switch
    persisten antar "restart" ( SqlitePolicyState ), konfirmasi Telegram
    timeout default-tolak, simulator endpoint mati error jelas.
- Config: tidak ada perubahan skema ( `policy.toml` telegram section sudah
  ada dari Fase 2 ). Passphrase keystore dari env ( bukan chat ).
- Dep Fase 4 via `[workspace.dependencies]` ( aturan #9 ): `teloxide` 0.15
  ( rustls+ctrlc_handler+macros ), `totp-rs` 5 ( gen_secret+zeroize ).
  Alloy tambah feature `signer-mnemonic` ( coins-bip39, §3A derivasi ).
  Crate langsung: `sqlx`, `hex`, `rand`, `zeroize`, `chrono`, `tracing`.
- **Verifikasi terakhir sukses**: `cargo fmt --all && cargo clippy
  --all-targets -- -D warnings && cargo test --workspace` → **166 test
  hijau** ( 38 core + 11 keystore + 13 policy + 6 signer + 29 chain unit +
  5 chain integration + 5 mint + 54 telegram unit + 5 telegram integration ),
  0 gagal, 0 warning.

### ✅ Fase 5 — Mint Engine & Eksekusi: SELESAI, semua Acceptance lulus

- **Foundry terpasang** ( binary release v1.3.2 di `.foundry/` —
  `anvil.exe`, `forge.exe`, `cast.exe`; `.gitignore` diupdate ).
  `cargo install --locked foundry` TIDAK berfungsi ( crate foundry adalah
  library, bukan binary ) — pakai release GitHub saja.
- **`crates/mint`** ( 31 unit test + 6 integration test fork Anvil ):
  - `sale.rs` — `SaleReader`: deteksi fungsi mint on-chain via RPC
    ( probe `eth_call` ke selector `mint()`/`mint(uint256)`/
    `mint(uint256,uint256)`/`publicMint(uint256)`/`freeMint(uint256)`/
    `safeMint(address,uint256)`/`mintTo(address,uint256)` — variant
    pertama yang non-revert menang ), baca `price()`, `maxPerWallet()`,
    `maxSupply()`, `totalSupply()`, `saleActive()`, `balanceOf(address)`,
    `ownerOf(uint256)` ( §1 F1, §5 poin 2 — semua dibaca langsung dari
    kontrak, bukan halaman web ).
  - `plan.rs` — `compile_exec_plan` ( §8.3 ): tx varian pre-signed 3 tier
    gas per wallet ( §7 Normal/Fast/Aggressive ), `reserve_nonces` via
    `NonceManager`, validasi qty <= `maxPerWallet` & total <= sisa supply
    & sale aktif. **Tidak menandatangani apa pun** — hanya menyusun
    `TxRequest` mentah ( aturan keras #2 ).
  - `trigger.rs` — `wait_for_trigger` ( §7 ): Timestamp ( Tsend =
    T0 - latency_p95 - margin - propagasi block ), Block ( via `newHeads`
    stream ), State ( `saleActive()` dibaca tiap block baru ).
  - `race.rs` — `race_broadcast` ( §8.3, §8.2 poin 6 ): kirim tx ke
    beberapa target ( public RPC pool + relay privat ) **paralel**,
    yang pertama OK menang ( `select_ok` ), timeout mutlak.
  - `exec.rs` — `MintExecutor`: `execute` ( pilih tier tercepat per
    wallet → `policy_check` → `sign_approved` → `race_broadcast` ),
    `confirm_and_verify` ( §6.3: tunggu receipt status 0x1, cek
    `balanceOf` bertambah, `ownerOf` menunjuk wallet, kumpulkan
    `gasUsed` ), `bump_or_cancel` ( §7: RBF +12.5% via `bump_fee` ), dan
    **default dry-run** ( aturan keras #3 — `MintTxStatus::DryRun` ).
  - `persist.rs` — idempotency key per ( task, wallet, nonce ) di tabel
    `txs` ( insert duplikat → `PersistError::Duplicate`, §6.2 "Dobel
    mint tak sengaja" ) + `pending_txs` untuk resume setelah restart.
  - Integration test ( `crates/mint/tests/integration.rs` ) terhadap
    **fork Anvil live** ( `http://127.0.0.1:8545`, chain 31337 ):
    mendeploy `TestNFT.sol` via `forge create --broadcast`, verifikasi
    4 acceptance Fase 5:
    1. `acceptance_fase5_mint_terjadi_dalam_satu_block` — mint non-dry-run
       terkirim, masuk block, `balanceOf` = 2 & `ownerOf` menunjuk burner.
    2. `acceptance_fase5_tidak_ada_dobel_mint_saat_restart` — idempotency
       key menolak eksekusi ganda.
    3. `acceptance_fase5_limit_per_wallet_dihormati` — qty 4 > maxPerWallet
       3 ditolak.
    4. `acceptance_fase5_pipeline_lengkap` + `sale_tidak_aktif_ditolak` +
       `trigger_timestamp_tsend_konsisten`.
    Diskip otomatis bila Anvil tak terjangkau ( `NFT_SKIP_FORK_TESTS=1` ).
    **Jalankan dengan `RUST_TEST_THREADS=1`** ( test deploy pakai nonce
    deployer Anvil yang sama → bentrok jika paralel ).
- **Bug ditemukan & diperbaiki**: `Broadcaster::broadcast` &
  `race_broadcast` memakai `alloy::rlp::Encodable::encode(envelope)` yang
  menghasilkan RLP **string wrapper** ( ditolak node: "Failed to decode
  transaction" ). Diganti `Encodable2718::encoded_2718()` ( EIP-2718:
  `type_byte || rlp(tx)` ) — sekarang diterima node.
- Contract uji `crates/mint/contracts/TestNFT.sol` — `mint(uint256)`
  dengan `price()`, `maxPerWallet()`, `maxSupply()`, `totalSupply()`,
  `saleActive()`, `balanceOf`, `ownerOf`. Pengecekan `saleActive` hanya
  berlaku saat `qty > 0` supaya probe deteksi ( qty = 0 ) tidak revert.
- Dep Fase 5 via `[workspace.dependencies]` ( aturan #9 ): **tidak ada
  crate baru** — hanya `nft-policy` + `futures-util` + `sqlx` + `chrono`
  ditambahkan ke `nft-mint` ( semuanya sudah ada di workspace ).
- **Verifikasi terakhir sukses**: `cargo fmt --all && cargo clippy
  --all-targets -- -D warnings && RUST_TEST_THREADS=1 cargo test
  --workspace` → **198 test hijau** ( 38 core + 11 keystore + 13 policy +
  6 signer + 29 chain unit + 5 chain integration + 31 mint unit +
  6 mint integration + 54 telegram unit + 5 telegram integration ),
  0 gagal, 0 warning.

- Windows (Git Bash/MSYS), Rust toolchain `stable` terpasang (rustc 1.99).
- Root proyek: `D:\nft agent battleground`.
- RPC publik Sepolia & mainnet terjangkau dari mesin ini (publicnode, 1rpc).
- **Foundry 1.3.2** terpasang di `.foundry/` ( `anvil.exe`, `forge.exe`,
  `cast.exe` ) via binary release GitHub — `cargo install --locked foundry`
  tidak berfungsi ( library crate ). Untuk test Fase 5+:
  `& ".\.foundry\anvil.exe" --port 8545 --block-time 1` lalu
  `$env:RUST_TEST_THREADS="1"; cargo test -p nft-mint --test integration`.
- **Penting**: jalankan integration test fork dengan `RUST_TEST_THREADS=1`
  ( test deploy memakai nonce deployer Anvil yang sama → bentrok jika
  paralel ).
- **Penting untuk agent berikutnya**: saat menulis file Rust panjang yang
  mengandung karakter non-ASCII ( emoji, §, — ), teloxide update, dll,
  validasi encoding setelah menulis. Beberapa file sempat mengalami
  mojibake ( UTF-8 double-encoded ) yang menyebabkan string literal rusak;
  sudah dibersihkan ke ASCII-safe ( emoji diganti `[OK]`, `[X]`, `[STOP]` ).
  Prefer ASCII-only dalam string literal untuk menghindari masalah ini.

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

## Target berikutnya: Fase 6 (Hari 22-27) — Link-to-Mint, Preflight, Reliability

Urutan pengerjaan yang disarankan:

1. `crates/linkresolver`: handler OpenSea Drops, Zora, Manifold,
   mint.fun (+ fallback website). `resolve_mint_link` ( §8.3, §5 poin 1 ).
2. `verify_contract` ( §5 poin 2 ): kontrak ter-verifikasi di explorer,
   deteksi proxy, skor risiko link ( domain typosquat, kontrak baru,
   `setApprovalForAll`/`delegatecall` mencurigakan, honeypot ).
3. `build_mint_plan` ( §5 poin 3 ) → ringkasan + tombol Konfirmasi di
   Telegram ( `crates/telegram` ).
4. `crates/preflight`: semua cek di §6.1 ( saldo >= harga x qty + gas
   cadangan 30%, nonce lokal = on-chain, >= 3 RPC sehat, offset jam,
   simulasi, allowlist/merkle proof, limit, gas di bawah cap, sesi login,
   policy ) — jadwal otomatis T-10 menit dan T-1 menit, `/preflight`.
5. Acceptance Fase 6 ( blueprint §9 ): 10 link uji ( benar +
   scam/typosquat ) diproses benar; link scam tidak pernah jadi tx;
   preflight merah blokir war; kegagalan disimulasikan ( RPC down, nonce
   nyangkut, saldo kurang ) tertangani sesuai tabel §6.2.

## Checklist Acceptance tiap fase (ringkas, dari §9)

- F1 ✅ — `cargo test` hijau; config invalid ditolak dengan pesan jelas.
- F2 ✅ — batas policy & signer type-level; kunci tidak muncul di log.
- F3 ✅ — transfer Sepolia full pipeline; revert terdeteksi di simulasi;
  `measure_latency`. ( Pipeline transfer via `Broadcaster` dry-run +
  integration test live RPC hijau; fork Anvil menyusul Fase 5. )
- F4 ✅ — ack < 300 ms ( diukur di `ack_dibawah_300ms_target` ); user
  non-whitelist diabaikan tanpa balasan; `/stop` < 2 detik (
  `stop_mematikan_kill_switch_dibawah_2_detik` ); phrase dari `/import`
  tidak muncul di log/SQLite/balasan ( `process_import_berhasil_setelah_konfirmasi`,
  `keystore_tidak_mengandung_phrase_plaintext` ), pesan dihapus (
  `deleteMessage` di `spawn` ), import ditolak bila saldo melewati batas
  ( `finalize_import_menolak_saldo_di_atas_burner` ).
- F5 ✅ — fork Anvil: mint < 1 block; tidak dobel mint saat restart; dry-run tidak kirim tx.
  ( `acceptance_fase5_mint_terjadi_dalam_satu_block` — tx masuk block & balanceOf/ownerOf
  terverifikasi; idempotency key menolak dobel eksekusi; `MintTxStatus::DryRun` tidak
  mengirim. )
- F6 — 10 link uji diproses benar; link scam tidak pernah jadi tx; preflight merah blokir war.
- F7 — tanpa password mentah; sign setApprovalForAll asing ditolak; SIWE domain salah ditolak.
- F8 — rule buy/list end-to-end testnet; PnL benar 3 skenario.
- F9 — ganti model tanpa ubah kode; prompt injection tidak menghasilkan intent berbahaya.
- F10 — job tetap jalan setelah restart; API localhost-only.
- F11 — event high/critical sampai WhatsApp tanpa rahasia; fallback Telegram teruji.
- F12 — target latensi §8.1 tercapai; 100 simulasi war tanpa kegagalan sisi-agent.
- F13 — audit trail, `cargo audit`, deploy VPS, uji bertahap 3 hari dry-run dulu.

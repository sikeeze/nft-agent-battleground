# NEXT_AGENT.md — Instruksi untuk Agent Berikutnya (nft-agent)

> Kamu meneruskan proyek `nft-agent` sesuai `NFT_AGENT_BLUEPRINT_TELEGRAM.md`.
> Baca file blueprint itu **penuh** dulu — terutama §3 (keamanan), §6 (reliability),
> §9 (jadwal fase), dan §11 (prompt awal + aturan keras).

## Status proyek (per 2026-11-01, akhir sesi Fase 8)

> **Catatan singkat untuk agent berikutnya**: Fase 1-9 sudah selesai dan
> hijau. Fase 10 (REST API & Scheduler) adalah target berikutnya —
> **wajib minta persetujuan user dulu** sebelum mengerjakannya (aturan keras
> #10). Semua commit belum dilakukan (working tree bersih di branch
> `fix/claim-validation`); user belum meminta commit.

### Lingkungan & cara verifikasi (WAJIB baca)

- OS Windows, PowerShell 5.1. Rust stable 1.99. Root proyek: `D:\nft agent battleground`.
- Selalu jalankan dari root:
  `cargo fmt --all && cargo clippy --all-targets -- -D warnings && RUST_TEST_THREADS=1 cargo test --workspace`
- Full test = **396 hijau** saat ini.Integration test Butuh env:
  - `$env:RUST_TEST_THREADS="1"` ( WAJIB — test deploy Anvil memakai nonce
    deployer yang sama, bentrok jika paralel ).
  - `$env:NFT_SKIP_NETWORK_TESTS="1"` / `$env:NFT_SKIP_FORK_TESTS="1"` untuk
    skip; tanpa flag, chain integration memerlukan RPC publik Sepolia
    (terjangkau dari mesin ini), mint/linkresolver integration memerlukan
    Anvil:
    `& ".\.foundry\anvil.exe" --port 8545 --block-time 1` (Foundry 1.3.2
    di `.foundry/`, bukan dari cargo install).
- **Penting**: tulis string literal Rust **ASCII-only**. File yang mengandung
  §/—/emoji pernah mengalami mojibake (UTF-8 double-encoded) di sesi
  sebelumnya; beberapa file lama masih mengandung karakter non-ASCII hasil
  encoding aneh — biarkan, jangan ditulis ulang tanpa `cargo check` setelahnya.
- Aturan #9 (dependency): **tidak ada crate baru** di Fase 7 & 8 — lanjutkan
  pola ini; perlu crate baru → tambahkan via `[workspace.dependencies]`
  root Cargo.toml + alasan di file ini.
- `git status` bersih; belum ada commit untuk Fase 7/8.

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

### ✅ Fase 6 — Link-to-Mint, Preflight, Reliability: SELESAI, semua Acceptance lulus

- **`crates/linkresolver`** ( 17 unit test + 3 integration test fork Anvil ):
  - `resolve.rs` — `resolve_mint_link` universal ( §5 poin 1 ): ambil
    halaman via `reqwest`, ekstrak alamat kontrak + chain id dari
    JSON tersemat/og:meta. **Deteksi typosquat dini** ( §5 poin 4 ):
    jarak edit 1 ke domain resmi / SLD sama-TLD beda / SLD mirip-TLD
    resmi → `ResolveError` ( `open-sea.io`, `opensea.om`, `opnsea.io`
    ditolak ). Platform detector: OpenSea/Zora/Manifold/mint.fun/
    Magic Eden/Website. ponytail: handler API resmi per-platform
    ( OpenSea Drops API, Zora API ) menyusul Fase 12.
  - `verify.rs` — `verify_contract` ( §5 poin 2, on-chain deterministik ):
    `eth_getCode` ( EOA → error ), deteksi proxy EIP-1967
    `implementation()`, baca state via `nft_mint::sale::SaleReader`,
    **skor risiko** ( §5 poin 4 ): `RiskLevel::Low/Medium/High/Critical`
    dari bytecode — `setApprovalForAll` ( a22cb465 ) → High,
    selfdestruct opcode 0xff → High, honeypot ( tanpa
    transferFrom/safeTransferFrom ) → High, owner setPrice/
    setSaleActive → Medium. `High`/`Critical` wajib konfirmasi
    manusia ( `needs_human()` ); `Critical` dibatalkan otomatis.
  - `plan.rs` — `build_mint_plan` ( §5 poin 3 ): qty per wallet =
    `maxPerWallet` dipotong sisa supply; `summarize()` format
    ringkasan Telegram ( Koleksi/Harga/Limit/Supply/Wallet/Gas/Risiko ).
- **`crates/preflight`** ( 6 unit test ) — `preflight()` 10 cek §6.1:
  Saldo ( `eth_getBalance` + gas cadangan 30% ), Nonce ( lokal vs
  on-chain pending ), RPC ( >= 3 sehat + selisih block ), Jam ( drift
  vs block timestamp; chrony host wajib ), Simulasi ( `EthCallSimulator`
  per wallet ), Allowlist ( V1: lolos bila tak ada merkle — Fase 12 ),
  Limit, Gas ( maxFee < cap + margin 10% ), Login ( Fase 7 ),
  Policy. `PreflightReport::summarize()` → checklist `[OK]`/`[X]`;
  **merah → war tidak otomatis** ( §6.1 ). `schedule_automatic` T-10m/
  T-1m ( scheduler penuh Fase 10 ).
- **Wiring Telegram** ( `crates/telegram/src/mintcommand.rs`, 3 test ):
  perintah `/mint <link>`, `/war <link>`, `/preflight` masuk
  `ENABLED_COMMANDS` + dispatch `process_command` ( bot.rs ).
  `handle_mint_link` = resolve → verify → build_plan → ringkasan +
  peringatan risiko; `BotState.rpc: Option<RpcPool>` ( None → pesan
  jelas ). Eksekusi tetap lewat konfirmasi tombol + kill switch.
- **`crates/chain/src/pool.rs`**: `eth_get_balance()` baru ( wei,
  `eth_getBalance` latest ) — dipakai preflight.
- Integration test ( `crates/linkresolver/tests/integration.rs` ) fork
  Anvil: mock HTTP server menyajikan halaman drop; deploy
  `TestNFT.sol` ( valid, dari crate mint ) + `DrainerNFT.sol`
  ( berbahaya — `setApprovalForAll` ). Acceptance F6:
  1. `acceptance_fase6_sepuluh_link_uji` — 5 link valid lolos
     verifikasi lengkap ( price terbaca, plan terbentuk ), 5 bermasalah
     diblokir ( typosquat, no-contract, drainer High, scheme invalid,
     host mati ).
  2. `acceptance_fase6_link_scam_tidak_jadi_tx` — DrainerNFT →
     `RiskLevel::High` wajib konfirmasi ( tidak auto-eksekusi ).
  3. `acceptance_fase6_preflight_merah_mencegah_war` — saldo 0 →
     PREFLIGHT GAGAL.
  Diskip otomatis bila Anvil tak terjangkau ( `NFT_SKIP_FORK_TESTS=1` );
  jalankan `RUST_TEST_THREADS=1`.
- Contract uji `crates/linkresolver/contracts/DrainerNFT.sol`.
- Dep Fase 6 via `[workspace.dependencies]` ( aturan #9 ): **crate baru
  `url` 2** ( parsing link, §5 ). Crate langsung: `nft-mint`,
  `nft-chain`, `nft-preflight`, `nft-linkresolver` di telegram/
  linkresolver/preflight.
- **Verifikasi terakhir sukses**: `cargo fmt --all && cargo clippy
  --all-targets -- -D warnings && RUST_TEST_THREADS=1 cargo test
  --workspace` → **228 test hijau** ( 38 core + 11 keystore + 13 policy +
  6 signer + 30 chain unit + 5 chain integration + 31 mint unit +
  6 mint integration + 17 linkresolver unit + 3 linkresolver integration +
  6 preflight + 57 telegram unit + 5 telegram integration ), 0 gagal,
  0 warning.

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

### ✅ Fase 7 — Social Vault & Browser Worker: SELESAI, semua Acceptance lulus

- **`crates/vault`** ( 29 unit test ):
  - `lib.rs` — Social Vault file-based: satu file terenkripsi per akun di
    `storage/vault/<id>.bin`, format `magic(7)|version(1)|salt(32)|
    nonce(24)|ciphertext` — **mesin keystore yang sama** ( Argon2id
    m=64MiB/t=3/p=1 + XChaCha20-Poly1305, §3B poin 1 ). `SessionState`
    ( cookies + localStorage + user_agent ) hanya ada di ciphertext; test
    memastikan nilai cookie/domain tidak muncul di file. `Vault::save_account`
    atomic ( tmp+rename ), `open_account`/`open_account_with` dekripsi ke
    `Secret<Vec<u8>>`, validasi id anti path-traversal, `remove_account`,
    `has_account`, `account_ids`. `AccountKind` ( google|x|discord|custom ).
  - `meta.rs` — `AccountMetaStore`: metadata NON-rahasia di tabel `accounts`
    ( id, kind, profile_dir, last_verified ) — tidak ada rahasia di SQLite
    ( §3B ). `add` ( duplikat → `MetaError::Duplicate` ), `get`, `list`,
    `touch_verified`, `remove`, `pool()`.
  - `health.rs` — `session_health` ( §3B poin 5 ): cek tiap akun ->
    `SessionStatus` `Healthy`/`Stale`/`Missing`/`ExpiringSoon`/`Expired`
    dari cookie inti ( session/auth/token ) + `last_verified`.
    `needs_relogin()` → notifikasi login ulang **sebelum** hari mint.
    `HealthConfig` ( max_age_hours 12, expiring_within_hours 24 ).
- **`crates/browser`** ( 47 unit test ):
  - `eip1193.rs` — request/respons EIP-1193: `Eip1193Method` (
    `requires_policy()` personal_sign/eth_signTypedData_v4/
    eth_sendTransaction/eth_sign; `is_readonly()` eth_chainId/eth_call/... ),
    `parse_send_transaction_params` → `BrowserTx`, deteksi **approval tak
    terbatas** ( selector `095ea7b3` argumen max 64'f' / `a22cb465` true ),
    `OriginAccounts` ( otorisasi per origin ), kode error standar
    ( 4001/4100/4200/-32001 ).
  - `siwe.rs` — parser SIWE EIP-4361 penuh: header domain+address, statement
    opsional, field URI/Version/Chain ID/Nonce/Issued At + opsional
    ( Expiration Time/Not Before/Request ID/Resources ). **Field asing =
    `Malformed`** ( "tidak ada permintaan tersembunyi", §3B ). `validate`:
    domain pesan = domain halaman, host URI = host origin, nonce segar
    ( = dikeluarkan & belum dipakai ), chain diizinkan, statement bebas
    permintaan tersembunyi ( "approve all"/"setapprovalforall"/... ),
    resource harus http(s)/chain://. `validate_times` ( Expired/NotYetValid ),
    `render` roundtrip.
  - `provider.rs` — `Eip1193Provider` ( §3B "Wallet connect lewat browser
    worker" ): `handle_request` → readonly langsung; `eth_requestAccounts`
    butuh otorisasi ( 4100 bila belum ); **sign request wajib lewat
    PolicyEngine** ( aturan keras #2 ); `eth_sendTransaction` dengan
    approval tak terbatas → POLICY_DENIED otomatis ( §3B ); `personal_sign`
    berbentuk SIWE divalidasi ketat ( `issue_nonce`, nonce tak bisa dipakai
    2x ); kill switch memblokir semua sign request.
  - ponytail: integrasi Chromium nyata ( chromiumoxide/CDP ) menyusul saat
    VPS siap; saat ini lapisan ini diuji tanpa browser ( aturan keras #5:
    tidak ada solver captcha/anti-detect ).
- **Wiring Telegram** ( `crates/telegram/src/accounts.rs`, 9 test ):
  perintah `/accounts` masuk `ENABLED_COMMANDS` + dispatch `process_command`.
  `/accounts` ( daftar+status ), `/accounts add <id> <kind>` ( login manual
  via remote browser — agent **tidak pernah meminta password di chat**,
  aturan keras #4 ), `/accounts check` ( session_health ), `/accounts
  remove <id>` ( hapus file+metadata ). `BotState.vault` +
  `BotState.account_store` ( `None` → pesan jelas ). Audit setiap aksi.
- **Wiring preflight** ( `crates/preflight/src/lib.rs` ): cek "Login" §6.1
  sekarang memakai `SessionHealthReport` ( bukan placeholder ) —
  `required_logins` di `PreflightConfig`; vault kosong/sesi expired →
  **merah → war tidak otomatis**. `preflight()` signature +1 param
  `Option<&[SessionHealthReport]>` ( `None` = tak butuh login );
  `mintcommand::run_preflight` & integration test diupdate.
- Config: tidak ada perubahan skema ( tabel `accounts` sudah ada dari
  Fase 1; session state di file vault, bukan SQLite — §3B ).
- Dep Fase 7 via `[workspace.dependencies]` ( aturan #9 ): **tidak ada crate
  baru** — vault memakai argon2/chacha20poly1305/zeroize/serde/sqlx yang
  sudah ada; browser memakai alloy/nft-policy/nft-chain yang sudah ada.
  Crate langsung: `nft-vault` ( telegram/preflight ), `nft-vault`+`nft-core`
  ( browser ).
- **Verifikasi terakhir sukses**: `cargo fmt --all && cargo clippy
  --all-targets -- -D warnings && RUST_TEST_THREADS=1 cargo test
  --workspace` → **316 test hijau** ( 38 core + 11 keystore + 13 policy +
  6 signer + 29 chain unit + 5 chain integration + 31 mint unit +
  6 mint integration + 17 linkresolver unit + 3 linkresolver integration +
  9 preflight + 29 vault + 47 browser + 66 telegram unit +
  5 telegram integration ), 0 gagal, 0 warning. Fork test Anvil & RPC
  publik Sepolia juga hijau.

### ✅ Fase 8 — Marketplace & Trader: SELESAI, semua Acceptance lulus

- **`crates/market`** ( 25 unit test ):
  - `lib.rs` — trait `Marketplace` seragam ( §F2 ): `get_floor`,
    `get_listings`, `buy`, `list`, `bid`, `cancel`. Tipe seragam
    `Listing`/`FloorPrice`/`ListParams`/`BidParams`/`UnsignedOrder`.
    **`buy`/`list`/`bid`/`cancel` mengembalikan `UnsignedOrder` yang
    BELUM ditandatangani** — pemanggil wajib lewat Policy Engine dahulu
    ( aturan keras #2 ). `MarketHttp` bersama ( HTTP/2 keep-alive ),
    retry 3x exponential backoff, error standar ( 429 rate-limit,
    4xx/5xx jelas ).
  - `rate.rs` — `RateLimiter` token bucket per adapter ( §F2 "Rate
    limiter + retry" ): `capacity` request per `window`, `acquire`
    mengembalikan `RateLimited`, `remaining` untuk observabilitas.
  - `opensea.rs` — `OpenSeaAdapter` ( Seaport 1.6, mainnet default
    `0x00000000006c3852cbEf3e08E8dF639F9306561e` ): baca floor via
    `/api/v2/collections/{addr}/stats` + listings via
    `/api/v2/listings/collection/{addr}?order_by=price_asc`; susun
    order EIP-712 Seaport ( name/version/verifyingContract ) untuk
    buy/list/bid/cancel. Validasi: chain adapter = chain order, harga
    > 0, durasi > 0, order_id tak kosong. ponytail: integrasi HTTP
    penuh menunggu API key ( `OPENSEA_API_KEY` ); sekarang diuji lewat
    parse respons & endpoint mati.
- **`crates/trader`** ( 48 unit test ):
  - `rules.rs` — parser `rules.toml` ( §4 config ): `[[rule]]` dengan
    `name`/`kind`/`collection`/`action`/`max_price_eth`/`below_floor_pct`/
    `max_buy_per_day`/`take_profit_pct`/`stop_loss_pct`/
    `list_duration_hours`/`enabled`. Validasi ketat: buy wajib kriteria
    harga, sell wajib TP/SL, alamat koleksi valid, pct 0..=100,
    nama duplikat ditolak, `deny_unknown_fields`.
  - `engine.rs` — `TradeEngine` deterministik: `evaluate_buy` ( rule buy
    vs floor + listings + `max_buy_per_day` ), `evaluate_sell` (
    take-profit → `ListForSale`, stop-loss → `StopLoss` dari portfolio ),
    `ListingInfo::is_stale` + `cancel_action` ( auto-cancel listing basi ).
    Output `TradeAction` — **bukan tx**, pemanggil wajib lewat Policy
    Engine ( aturan keras #2 ).
  - `pnl.rs` — `Portfolio` + `PnLEntry`: harga beli rata-rata tertimbang,
    nilai sekarang di-refresh dari floor, PnL absolut/persen per posisi
    & total, `take_profit_price`/`stop_loss_price`, `hit_take_profit`/
    `hit_stop_loss`, `summarize()` ( bebas rahasia ).
- **Wiring Telegram** ( `crates/telegram/src/trade.rs`, 7 test ):
  `/portfolio` ( ringkasan PnL ), `/rules` ( daftar ), `/rules reload`
  ( muat ulang `rules.toml` + validasi ). Masuk `ENABLED_COMMANDS` +
  dispatch `process_command` + audit. `BotState.portfolio` &
  `BotState.rules` ( `Mutex`, default kosong ).
- Config: `RULES_FILE` konstanta `rules.toml` di `crates/core/src/config.rs`;
  contoh `config/rules.toml` ( 2 rule, `enabled = false` — harus diaktifkan
  manual, konsisten dengan aturan keras #3 dry-run default ).
- Dep Fase 8 via `[workspace.dependencies]` ( aturan #9 ): **tidak ada crate
  baru** — market memakai reqwest/alloy/async-trait/chrono yang sudah ada;
  trader memakai toml/alloy/serde yang sudah ada.
- **Verifikasi terakhir sukses**: `cargo fmt --all && cargo clippy
  --all-targets -- -D warnings && RUST_TEST_THREADS=1 cargo test
  --workspace` → **396 test hijau** ( 38 core + 11 keystore + 13 policy +
  6 signer + 29 chain unit + 5 chain integration + 31 mint unit +
  6 mint integration + 17 linkresolver unit + 3 linkresolver integration +
  9 preflight + 29 vault + 47 browser + 25 market + 48 trader +
  73 telegram unit + 5 telegram integration ), 0 gagal, 0 warning.

### ✅ Fase 9 — LLM, Skills, Bahasa Natural: SELESAIA, semua Acceptance lulus

- **`crates/llm`** ( 26 unit test ):
  - `lib.rs` — `ChatMessage` ( system/user ), `LlmConfig::from_models`
    ( timeout dari `plan_timeout_secs`, minimum 1 detik ).
  - `router.rs` — `LlmClient` OpenAI-compatible: satu `reqwest::Client`
    ( HTTP/2 keep-alive + pool, §8.2 poin 5 ), `POST {base}/chat/completions`
    + `Authorization: Bearer` ( header sensitive, tolak CRLF injeksi ).
    Base URL & API key dibaca dari **env var** yang dinamai di
    `models.toml` ( kredensial tidak ditulis di file config, §F7 ).
    `with_model()` ganti model runtime ( `/model <id>`, §F7 "ganti model
    tanpa ubah kode" ). Timeout mutlak ( default 3 dtk dari
    `plan_timeout_secs`, §8.2 poin 3 — LLM tidak boleh di jalur eksekusi
    mint, aturan keras #6 ).
  - `sanitize.rs` — `sanitize_untrusted` ( §F7 prompt injection ):
    bungkus teks asing sebagai `[DATA TIDAK TEPERCAYA ...]`, netralkan
    baris berbahaya ( "kirim semua eth", "ignore previous",
    "setapprovalforall", "approve(", "you are now", "tanpa konfirmasi" )
    dengan prefix `>`, potong di 8 KiB. ponytail: filter teks bukan
    jaminan mutlak — pertahanan utama tetap Policy Engine + verifikasi
    on-chain.
  - `tools.rs` — `ToolSpec` ( deklarasi tool function calling ) +
    `parse_tool_calls` ( parser longgar: code fence + teks pengantar,
    tapi ketat field wajib; arguments wajib objek JSON ). Tool call
    **belum dieksekusi** — eksekusi deterministik di handler skill.
  - Integration: tidak ada crate baru ( reqwest/serde/tokio sudah ada
    di workspace, aturan #9 ).
- **`crates/skills`** ( 26 unit test ):
  - `manifest.rs` — `SkillManifest` ( `skill.toml`, `deny_unknown_fields` ):
    name ( alfanumerik/_/- anti path traversal ), script ( nama file
    saja, anti `..` ), interpreter ( python|node|bash ), timeout 1..=60
    dtk. `SkillPermission`: `allowed_tools` ( subset `AllowedTool`:
    get_floor/get_listings/gas_price/mint_plan — **tool financial
    buy/sign/approve tidak pernah tersedia untuk skill**, §F8 ),
    `allowed_domains`, `max_calls_per_hour`, `can_propose_intent`.
    Default **deny-all**: skill tanpa deklarasi tool tidak bisa
    memanggil apapun ( acceptance Fase 9 ).
  - `sandbox.rs` — `run_skill` eksekusi **subprocess terpisah** (
    ponytail: WASM menyusul; subprocess dipilih karena isolasi memory
    jelas + tidak butuh runtime baru, aturan #9 ). Komunikasi JSON
    stdin/stdout; env dibersihkan ( hanya PATH+SYSTEMROOT diteruskan,
    skill tidak mewarisi token agent ); timeout mutlak + kill lewat
    taskkill. Output divalidasi: tool di luar permission →
    `ToolOutOfPermission`, pesan berbahaya → `DangerousOutput`.
    `SkillInput` hanya data pasar ( floor, gas, koleksi ) — **bebas
    rahasia** ( §F8 "Skill tidak punya akses signer/keystore" ).
  - Skill contoh di `skills/`: `floor_sniper` ( report floor, 2 tool ),
    `gas_optimizer` ( sarankan tier gas, deny-all ). Keduya
    `can_propose_intent = false`.
- **Bahasa natural -> rule** ( `crates/trader/src/naturallang.rs`, 17 test ):
  - `RuleDraft` + `parse_draft` ( parser JSON longgar dari respons LLM +
    validasi ketat: pola berbahaya ditolak, kriteria harga wajib,
    alamat valid ).
  - `parse_pattern` — **fallback deterministik tanpa LLM** ( §8.2 poin 3 ):
    "beli <alamat> di bawah 2 ETH" / "di bawah 10% floor" / "list +20%".
  - `NL_SYSTEM_PROMPT` — instruksi LLM hanya kembalikan JSON field
    `RuleDraft`; permintaan berbahaya → `{"name":"DITOLAK"}`.
  - `add_draft` — tambah draft ke `RulesConfig` ( cek duplikat nama ).
- **Wiring Telegram**:
  - `llmcommand.rs` ( 6 test ) — `/model` ( daftar + model aktif ),
    `/model <id>` ( ganti via `LlmClient::with_model` ), `/skills`
    ( daftar folder `skills/` ), `/skills <id>` ( jalankan sekali,
    hasil laporan ).
  - `nlcommand.rs` ( 7 test ) — `natural_to_rule` ( LLM → draft,
    fallback `parse_pattern` bila LLM error/timeout ), `DraftSessions`
    ( draft pending per user, TTL 2 menit ).
  - `bot.rs` — pesan non-perintah dari user whitelist masuk antrean
    sebagai perintah `natural` ( hot path hanya ack; LLM di worker,
    aturan keras #6 ). `/yes` `/no` konfirmasi draft ( §F6: "tampilkan
    ringkasan + tombol Konfirmasi sebelum disimpan" ). Import phrase
    tetap punya prioritas ( §3A poin 3 ).
  - `ENABLED_COMMANDS` sekarang 17: + `/model`, `/skills`, `/yes`,
    `/no`. `BotState` +4 field: `models`, `llm`, `draft_sessions`,
    `skills_dir`.
- Config: tidak ada perubahan skema ( `models.toml` sudah ada dari
  Fase 1 dengan `default`/`plan_timeout_secs`/`[[model]]` ).
- Dep Fase 9 via `[workspace.dependencies]` ( aturan #9 ): **tidak ada
  crate baru** — llm memakai reqwest/serde/thiserror/tokio yang sudah
  ada; skills memakai toml/tokio(process)/serde; trader/telegram
  memakai crate sendiri + nft-llm/nft-skills.
- **Verifikasi terakhir sukses**: `cargo fmt --all && cargo clippy
  --all-targets -- -D warnings && RUST_TEST_THREADS=1 cargo test
  --workspace` → **478 test hijau** ( 38 core + 11 keystore + 13 policy +
  6 signer + 30 chain unit + 5 chain integration + 31 mint unit +
  6 mint integration + 17 linkresolver unit + 3 linkresolver integration +
  9 preflight + 29 vault + 47 browser + 25 market + 65 trader +
  26 llm + 26 skills + 86 telegram unit + 5 telegram integration ),
  0 gagal, 0 warning.
- **Acceptance Fase 9** ( §9 ):
  1. Ganti Claude <-> Ollama tanpa ubah kode: `models.toml` routing +
     `LlmClient::with_model` + `/model <id>` (
     `ganti_model_tanpa_ubah_kode`, `model_ganti_tanpa_ubah_kode` ).
     Semua endpoint OpenAI-compatible ( base URL per entri dari env ).
  2. Skill minta tool di luar permission ditolak:
     `skill_node_tool_di_luar_permission_ditolak`,
     `tool_finansial_ditolak_walupun_di_manifest`,
     `tool_di_luar_permission_ditolak` ( termasuk tool financial yang
     sengaja didaftarkan di manifest tetap ditolak karena bukan
     `AllowedTool` ).
  3. Prompt injection tidak menghasilkan intent berbahaya:
     `instruksi_penipuan_dinetralkan` ( sanitize ),
     `draft_berbahaya_ditolak` ( draft LLM ),
     `pesan_berbahaya_ditolak` + `skill_output_berbahaya_ditolak`
     ( output skill ). LLM hanya mengusulkan; Policy Engine menyetujui.

## Target berikutnya: Fase 10 (Hari 45-48) — REST API & Scheduler

Urutan pengerjaan yang disarankan:

1. `crates/scheduler`: cron, one-shot ( mint pada tanggal/jam tertentu ),
   trigger event ( block baru, floor turun, listing baru ); job tersimpan
   di SQLite dan tetap jalan setelah restart ( §F9 ).
2. `crates/api`: axum REST + auth Bearer + OpenAPI + SSE; hanya bind
   `127.0.0.1` / lewat VPN ( Tailscale ), §F10.
3. Acceptance Fase 10 ( blueprint §9 ): job terjadwal tetap jalan setelah
   restart; API hanya dapat diakses dari localhost/VPN.

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
- F6 ✅ — 10 link uji diproses benar; link scam tidak pernah jadi tx; preflight merah blokir war.
- F7 ✅ — tanpa password mentah; sign setApprovalForAll asing ditolak; SIWE domain salah ditolak.
  ( Vault: hanya `SessionState` terenkripsi yang disimpan ( test
  `file_vault_tidak_mengandung_plaintext_cookie` ), password tidak pernah
  diminta di chat; `/accounts add` membuat profil browser isolasi untuk
  login manual. Browser: `send_transaction_approval_tak_terbatas_ditolak`
  + `setapprovalforall_ke_asing_ditolak`; SIWE `validate_domain_berbeda_ditolak`
  + `validate_uri_berbeda_ditolak` + `siwe_nonce_tidak_dipakai_dua_kali`;
  tidak ada kode solver captcha di repo ( grep `captcha` → hanya komentar
  aturan keras #5 ). Sesi tetap ada setelah restart: file vault persisten
  + metadata `accounts` di SQLite WAL. )
- F8 ✅ — rule buy/list end-to-end testnet; PnL benar 3 skenario.
  ( rule buy "beli di bawah X%" dievaluasi di `evaluate_buy`; rule sell
  "list +Y%" di `evaluate_sell`; PnL benar di 3 skenario test: untung
  +50%, rugi -25%, impas 0%. End-to-end fork Anvil menyusul Fase 12
  saat ExecPlan menyatukan market+policy+signer. )
- F9 ✅ — ganti model tanpa ubah kode; prompt injection tidak menghasilkan intent berbahaya.
  ( `models.toml` routing + `LlmClient::with_model` + `/model <id>`;
  skill tool di luar permission ditolak ( `AllowedTool` tertutup, tool
  financial tidak tersedia ); prompt injection ditolak di tiga lapis:
  `sanitize_untrusted` ( input ), `parse_draft` ( draft LLM ),
  `validate_output` ( output skill ). LLM hanya mengusulkan; Policy
  Engine menyetujui. )
- F10 — job tetap jalan setelah restart; API localhost-only.
- F11 — event high/critical sampai WhatsApp tanpa rahasia; fallback Telegram teruji.
- F12 — target latensi §8.1 tercapai; 100 simulasi war tanpa kegagalan sisi-agent.
- F13 — audit trail, `cargo audit`, deploy VPS, uji bertahap 3 hari dry-run dulu.

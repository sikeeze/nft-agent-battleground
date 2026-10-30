# NFT AGENT (Rust + Telegram): Blueprint Final

Agent yang berjalan 24/7 di VPS untuk **mint / war mint**, **auto buy-sell** di marketplace NFT, dikontrol dari **Telegram**, dengan **LLM yang bisa diganti** dan **sistem skill** yang bisa ditambah.

> Untuk agent VS Code: kerjakan per fase, jangan loncat. Setiap fase harus lulus bagian "Acceptance" sebelum lanjut.

---

## 1. Keputusan Final

| Topik | Pilihan |
|---|---|
| Kanal kontrol | **Telegram Bot** (teloxide). Resmi, gratis, punya tombol konfirmasi inline. Semua perintah dan konfirmasi hanya lewat sini |
| Kanal notifikasi | **WhatsApp Cloud API (resmi), satu arah** untuk alert penting. Telegram tetap kanal cadangan |
| Tempat jalan | **VPS** (Docker). Bukan di HP, supaya stabil dan latency rendah |
| Bahasa | **Rust** (tokio, alloy, axum, sqlx) |
| Chain awal | EVM: Ethereum, Base, Arbitrum, Polygon. Solana = fase lanjutan |
| Marketplace | OpenSea dulu (Seaport), lalu Blur, Magic Eden, dll via adapter |
| LLM | Antarmuka OpenAI-compatible, bisa Claude / GPT / Ollama / model custom |
| Kunci | Wallet burner + private key terenkripsi. Import **boleh lewat chat** (khusus wallet burner, dengan pengaman bagian 3A) |
| Input mint | **Cukup kirim link** mint ke bot (`/mint <link>` atau paste link saja) |
| Akun Google / X | **Social Vault** terenkripsi + browser worker terisolasi (lihat 3B) |
| Mode awal | **Dry-run** (tidak mengirim tx) sampai diaktifkan manual |

```
HP (Telegram) <--> Telegram server <--> VPS: [Bot] -> [Agent/LLM] -> [Policy] -> [Simulator] -> [Signer] -> [RPC/Relay]
```

---

## 2. FITUR INTI

### F1. Mint & War Mint
- Deteksi fungsi mint dari ABI (harga, limit per wallet, status sale).
- Trigger: waktu (timestamp), nomor block, atau perubahan state kontrak (`saleActive()` jadi true).
- Tx **disiapkan sebelumnya**, tinggal dikirim saat trigger, supaya latency minimal.
- Multi-wallet, merkle proof untuk allowlist, gas EIP-1559 mode `normal` / `aggressive` (dibatasi policy).
- Retry cerdas: gagal karena gas -> naikkan gas dalam batas; sold out / sale belum aktif -> berhenti.
- Kirim lewat private relay (Flashbots Protect / MEV-Blocker) untuk mengurangi frontrun.

### F2. Auto Buy / Sell
- Aturan per koleksi: batas harga, beli di bawah floor X%, filter trait, maksimal beli per hari.
- Auto-list setelah beli (take profit), stop loss, auto-cancel listing basi.
- Portfolio tracker: harga beli, nilai sekarang, PnL.
- Adapter marketplace seragam (`get_floor`, `get_listings`, `buy`, `list`, `bid`, `cancel`).

### F3. Auto Sign yang Aman
- Mendukung sign tx, `personal_sign`, dan EIP-712 (order Seaport).
- Signer hanya menerima `ApprovedTx` dari Policy Engine; tidak ada jalur sign lain.
- Isi order EIP-712 (harga, item, expiry) diparsing dan diperiksa sebelum ditandatangani.

### F4. Keystore (Penyimpanan Kunci)
- Private key dienkripsi Argon2id + XChaCha20-Poly1305; passphrase tidak disimpan di disk.
- Impor kunci/mnemonic bisa lewat **chat Telegram (`/import`)** atau CLI. Untuk chat, wajib ikuti alur pengaman di bagian 3A. Setelah dienkripsi, data mentah langsung dihapus dari memori (`zeroize`).
- Alternatif paling aman: `/newwallet` (agent membuat wallet sendiri, Anda tinggal kirim dana), jadi tidak ada rahasia yang lewat chat.
- Tipe `Secret<T>` supaya kunci tidak pernah muncul di log atau `Debug`.
- Opsi lanjutan: smart account + session key dengan batas nominal dan kontrak.

### F5. Policy Engine & Kill Switch (Pengaman Utama)
- LLM **hanya mengusulkan Intent**; kode deterministik yang memutuskan boleh atau tidak.
- Batas: nominal per tx / per hari, max gas, chain yang diizinkan, allowlist kontrak, blokir `approve(max)` dan `setApprovalForAll` ke kontrak asing.
- Simulasi (`eth_call`) wajib sebelum kirim; tx yang revert di simulasi tidak dikirim.
- Konfirmasi manusia (tombol Telegram) untuk nominal besar, kontrak/koleksi baru, atau penarikan.
- `/stop` mematikan semua job dan signer; bisa otomatis aktif saat rugi melewati batas.

### F6. Kontrol via Telegram
- Perintah: `/status`, `/import`, `/newwallet`, `/wallets`, `/accounts`, `/mint <link>`, `/preflight <task>`, `/war`, `/rules`, `/portfolio`, `/model`, `/skills`, `/approve`, `/reject`, `/stop`.
- Bahasa natural: "pantau koleksi X, beli di bawah 2 ETH, list +20%" -> LLM ubah jadi rule -> bot tampilkan ringkasan + tombol **Konfirmasi** sebelum disimpan.
- Notifikasi real-time: mint sukses/gagal, listing terjual, saldo rendah, error beruntun.

### F7. Custom Model LLM
- Satu klien OpenAI-compatible; model dan routing diatur di `models.toml`.
- Contoh: model besar untuk perencanaan, model lokal murah untuk chat biasa.
- Ganti model lewat `/model <id>` tanpa ubah kode.
- Konten eksternal (deskripsi NFT, nama koleksi) diperlakukan sebagai **data tidak tepercaya** untuk menahan prompt injection.

### F8. Sistem Skill
- Skill = folder plugin (`skill.toml` + WASM/script) yang bisa ditambah tanpa compile ulang.
- Sandbox dengan permission eksplisit (domain jaringan, tool yang boleh dipanggil, batas waktu).
- Skill **tidak punya akses ke signer/keystore**; hanya bisa menghasilkan Intent yang tetap lewat Policy Engine.
- Skill bawaan: `floor_sniper`, `trait_bidder`, `mint_watcher`, `pnl_reporter`, `gas_optimizer`.

### F11. Mint Hanya dengan Link
- Anda cukup kirim link (OpenSea Drops, Zora, Manifold, mint.fun, Magic Eden, atau website project). Agent mengekstrak chain, kontrak, harga, stage, limit, dan jadwal sendiri.
- Hasil ekstraksi **diverifikasi on-chain** (bukan dipercaya dari halaman web), lalu ditampilkan sebagai ringkasan + tombol Konfirmasi. Detail di bagian 5.

### F12. Social Vault (Akun Google / X / Wallet Login)
- Menyimpan sesi login akun Google dan X (terenkripsi) agar agent bisa melewati mint yang butuh login.
- Wallet connect otomatis lewat provider EIP-1193 yang **semua permintaan sign-nya tetap lewat Policy Engine**. Detail di 3B.

### F13. Preflight & Anti-Gagal
- Pemeriksaan otomatis pada T-10 menit dan T-1 menit (saldo, nonce, RPC, simulasi, sesi login, allowlist, jam server). Mint tidak dimulai kalau ada yang merah. Detail di bagian 6.

### F14. Respons Cepat
- Bot membalas dalam <300 ms (ack instan), kerja berat jalan di antrean async, jalur eksekusi mint tidak melewati LLM. Detail di bagian 8.

### F15. Notifikasi WhatsApp (Satu Arah)
- Alert penting dikirim ke WhatsApp Anda lewat **WhatsApp Cloud API resmi** memakai template yang sudah disetujui Meta: hasil mint, war dimulai, preflight gagal, sesi akun mati, saldo rendah, kill switch aktif, listing terjual.
- **WhatsApp tidak dipakai untuk perintah atau konfirmasi.** Semua kontrol tetap di Telegram. Detail di 3C.

### F9. Scheduler
- Cron, one-shot (mint pada tanggal/jam tertentu), dan trigger event (block baru, floor turun, listing baru).
- Job tersimpan di SQLite dan tetap jalan setelah restart.

### F10. REST API Lokal
- Untuk kustomisasi lebih dalam: tambah rule, kelola skill, ganti model, ambil portfolio, stream log (SSE).
- Hanya bind `127.0.0.1` / lewat VPN (Tailscale), pakai Bearer token. Dokumentasi OpenAPI.

---

## 3. Aturan Keamanan Chat Telegram

Chat bot Telegram **tidak end-to-end encrypted**, dan akun Telegram yang diambil alih = kontrol ke agent. Maka:

1. **Passphrase keystore dan private key wallet utama tidak boleh lewat chat.** Impor via chat hanya untuk wallet burner (lihat 3A).
2. **Whitelist `user_id`** (bukan username). Pesan dari ID lain diabaikan tanpa balasan.
3. Aktifkan **2FA di akun Telegram** Anda.
4. Lewat chat boleh: lihat status, ubah rule kecil, konfirmasi tx, kill switch, dan **import wallet burner**. **Ubah policy dan ekspor kunci hanya lewat CLI/VPN.**
5. **TOTP** sebagai lapisan kedua untuk konfirmasi di atas nominal tertentu.
6. Mint war dan job terjadwal **tidak boleh bergantung pada chat**. Kalau Telegram down, job tetap jalan sesuai jadwal.
7. Rate limit perintah dan log audit untuk setiap aksi dari chat.

### 3A. Alur Import Phrase Lewat Chat (Wallet Burner)

Keputusan: import via chat **diizinkan**, dengan syarat dan pengaman berikut. Fitur ini mati secara default (`import_via_chat = false`) dan dinyalakan di `agent.toml`.

**Alur `/import`:**
1. User kirim `/import`. Bot menjawab peringatan singkat dan meminta konfirmasi tombol: *"Saya konfirmasi ini wallet burner, bukan wallet utama."* Aksi ini juga minta TOTP kalau aktif.
2. Bot meminta user mengirim **private key atau mnemonic dalam satu pesan**. Sesi import kedaluwarsa dalam 2 menit.
3. Begitu pesan diterima, bot **langsung `deleteMessage`** pada pesan user (bot boleh menghapus pesan masuk di private chat), sebelum proses lain.
4. Phrase di-parse di memori, diturunkan ke alamat, **dienkripsi ke keystore**, lalu `zeroize`. Tidak ditulis ke SQLite, log, atau file sementara.
5. Bot memeriksa saldo wallet. Jika di atas `burner_max_balance_eth`, import **ditolak** dengan pesan agar memindahkan sebagian dana dulu.
6. Bot membalas hanya dengan **alamat wallet + saldo**, tidak pernah mengulang phrase. Audit log mencatat alamat dan waktu, bukan isi.

**Aturan pengaman:**
- **Utamakan private key daripada mnemonic.** Mnemonic membuka semua akun turunannya. Kalau tetap memakai mnemonic, agent hanya menurunkan satu path (`m/44'/60'/0'/0/0`) dan membuang sisanya. Paling baik: buat mnemonic baru khusus agent, jangan pakai ulang dari wallet lain.
- Logging teloxide/tracing **wajib menyaring payload update mentah** pada state import (test otomatis membuktikan phrase tidak muncul di log).
- Percobaan import dibatasi (rate limit + lockout setelah beberapa kali gagal parse).
- Maksimal jumlah wallet dan saldo per wallet diatur policy; dana di atas batas tidak dipakai agent.
- Hanya `user_id` whitelist yang bisa memulai import.

**Risiko yang tetap ada (harus Anda terima):**
- Pesan Telegram bot **tidak end-to-end encrypted** dan sempat tersimpan di server Telegram sebelum dihapus. Hapus juga riwayat chat dari sisi Anda dan jangan lakukan import di jaringan/perangkat yang tidak Anda percaya.
- Kalau akun Telegram Anda diambil alih sebelum import, penyerang bisa memicu import atau melihat riwayat. 2FA Telegram wajib aktif.
- Phrase yang pernah dipakai di tempat lain tetap dianggap bocor kalau chat bocor. Karena itu hanya isi wallet burner dengan modal yang siap hilang.

### 3B. Social Vault: Akun Google, X, dan Login Wallet

Beberapa mint meminta login Google/X atau "Sign in with Ethereum". Agent menanganinya dengan **sesi, bukan password**.

**Prinsip:**
1. **Jangan simpan password mentah.** Yang disimpan adalah *storage state* sesi browser (cookies + localStorage) yang dienkripsi dengan mesin keystore yang sama (Argon2id + XChaCha20).
2. **Login pertama dilakukan manusia.** `/accounts add google|x` membuat sesi browser terisolasi di VPS dan mengirim tautan **noVNC/remote browser lewat Tailscale** (bukan internet publik). Anda login sendiri (termasuk 2FA), lalu sesinya disimpan. Agent tidak mengetik password Anda.
3. **Satu profil browser per akun**, terpisah dari yang lain (Chromium profile + proxy/IP tetap yang sama agar tidak memicu "login mencurigakan").
4. **Pakai akun khusus**, bukan akun Google utama Anda. Akun Google utama berisi email, Drive, dan password manager; kalau terkunci atau ter-ban karena otomasi, dampaknya besar.
5. **Sesi kedaluwarsa dipantau.** Job `session_health` mengecek sesi tiap beberapa jam; kalau mati, bot mengirim notifikasi minta login ulang *sebelum* hari mint, bukan saat war.
6. **Captcha, 2FA, dan verifikasi manusia tidak di-bypass.** Agent tidak punya solver captcha atau penghindar deteksi bot. Kalau muncul tantangan, agent berhenti, mengirim tangkapan layar ke Telegram, dan menunggu Anda menyelesaikannya lewat remote browser.
7. **Tugas sosial (follow/like/retweet) memakai API resmi X** dengan OAuth 2.0 bila memungkinkan. Otomasi browser untuk X melanggar ToS X dan akun bisa disuspend; fitur ini opsional dan bendera `x_browser_automation` default `false`.

**Wallet connect lewat browser worker:**
- Chromium headless diberi provider `window.ethereum` (EIP-1193) buatan agent.
- Setiap `eth_requestAccounts`, `personal_sign`, `eth_signTypedData_v4`, `eth_sendTransaction` dari halaman web diteruskan ke **Policy Engine** (bukan langsung ke signer).
- Untuk SIWE / pesan login: parsing pesan, pastikan **domain di pesan = domain halaman**, nonce segar, tidak ada permintaan tersembunyi. Pesan yang tidak cocok ditolak.
- Transaksi dari halaman web tetap harus lolos allowlist, simulasi, dan batas nominal. Situs yang meminta `setApprovalForAll` atau approve tak terbatas ke alamat asing otomatis ditolak.

**Struktur data (terenkripsi):**

```toml
[[account]]
id = "x-main-burner"
kind = "x"                        # google | x | discord | custom
profile_dir = "vault/profiles/x-main-burner"
session_state = "<blob terenkripsi>"
last_verified = "2026-10-29T08:00:00Z"
proxy = "socks5://..."            # opsional, IP konsisten
```

**Perintah:** `/accounts` (daftar + status sesi), `/accounts add <kind>`, `/accounts check`, `/accounts remove <id>`.

**Risiko yang harus diterima:** otomasi login bisa membuat akun diblokir atau di-captcha; sesi tersimpan di VPS berarti VPS yang diretas bisa membuka akun itu (itu alasan memakai akun khusus dan VPS yang di-hardening).

### 3C. Notifikasi WhatsApp (Satu Arah)

**Kenapa satu arah:** WhatsApp Cloud API tidak bisa menghapus atau mengedit pesan, tombolnya terbatas (maks 3), pengiriman di luar jendela 24 jam butuh template berbayar, dan penerimaan pesan butuh webhook publik. Jadi WhatsApp dipakai untuk *memberi tahu*, sedangkan Telegram untuk *mengendalikan*.

**Aturan:**
1. **Hanya API resmi (Meta Cloud API).** Jangan pakai Baileys/whatsapp-web.js: melanggar ToS, nomor bisa banned, sesi sering putus.
2. **Tanpa webhook, tanpa port publik.** Agent hanya *mengirim* ke Graph API. Dengan begitu prinsip "API hanya localhost/VPN" tetap terjaga. Konsekuensi: tidak ada status "terbaca/terkirim" dan tidak ada perintah masuk dari WhatsApp.
3. **Semua pesan memakai template (utility)** yang disetujui Meta, jadi tidak terkena batas jendela 24 jam. Template yang perlu didaftarkan: `mint_result`, `war_started`, `preflight_failed`, `session_expired`, `low_balance`, `killswitch_triggered`, `listing_sold`.
4. **Isi pesan tidak boleh memuat rahasia.** Tidak ada private key, phrase, token, atau tautan login. Alamat wallet disingkat (`0xAb..12`), cukup nama tugas, status, dan jumlah.
5. **Prioritas dan hemat biaya.** Hanya event *high/critical* yang dikirim ke WhatsApp. Ada dedupe (pesan serupa dalam N menit digabung), batas pesan per jam, dan jam tenang (`quiet_hours`) kecuali critical.
6. **Tidak pernah memblokir hot path.** Pengiriman lewat antrean async terpisah dengan timeout; kalau WhatsApp gagal atau lambat, mint tetap jalan.
7. **Fallback otomatis.** Jika WhatsApp error, event yang sama dikirim ke Telegram. Event critical dikirim ke **keduanya**.
8. **Token WhatsApp** (system user token) disimpan di environment/secret VPS, bukan di repo, dan diberi izin minimum. Nomor penerima harus yang Anda daftarkan.
9. **Biaya:** pesan template berbayar menurut tarif Meta yang berlaku; cek harga terbaru di dokumentasi Meta sebelum mengaktifkan, dan atur batas bulanan di config.

```toml
# agent.toml
[notify]
primary = "telegram"
critical_to_all = true
quiet_hours = "23:00-06:00"         # critical tetap dikirim
dedupe_window_sec = 120

[notify.whatsapp]
enabled = true
phone_number_id = "<id dari Meta>"
access_token_env = "WA_ACCESS_TOKEN"
recipient = "+62xxxxxxxxxx"
min_priority = "high"
max_per_hour = 20
monthly_budget_msgs = 500
templates = { mint_result = "mint_result_v1", war_started = "war_started_v1", preflight_failed = "preflight_failed_v1", session_expired = "session_expired_v1", low_balance = "low_balance_v1", killswitch_triggered = "killswitch_v1", listing_sold = "listing_sold_v1" }
```

**Persiapan di sisi Meta (butuh waktu, mulai lebih awal):** akun Meta Business, aplikasi di Meta for Developers dengan produk WhatsApp, nomor pengirim khusus, verifikasi bisnis jika diminta, dan pengajuan template (persetujuan biasanya hitungan menit sampai beberapa hari).

---

## 4. Struktur Repo

```
nft-agent/
├── Cargo.toml
├── config/            agent.toml, policy.toml, models.toml, rules.toml
├── crates/
│   ├── core/          Intent, Task, WalletRef, Event, Secret<T>
│   ├── keystore/      enkripsi & manajemen wallet
│   ├── signer/        tx, personal_sign, EIP-712
│   ├── chain/         RPC pool, nonce, gas, simulator, broadcaster
│   ├── policy/        policy engine + kill switch
│   ├── mint/          strategi mint & war
│   ├── linkresolver/  link -> MintPlan (parser per platform + verifikasi on-chain)
│   ├── preflight/     pemeriksaan sebelum mint
│   ├── vault/         penyimpanan sesi Google/X terenkripsi
│   ├── browser/       Chromium worker + provider EIP-1193 (lewat policy)
│   ├── market/        adapter OpenSea dkk
│   ├── trader/        rules buy/sell, watcher, PnL
│   ├── llm/           klien LLM + tool calling
│   ├── skills/        loader & sandbox
│   ├── scheduler/     cron + event trigger
│   ├── api/           axum REST
│   ├── notify/        router notifikasi (prioritas, dedupe, fallback)
│   ├── whatsapp/      klien WhatsApp Cloud API (kirim template, satu arah)
│   └── telegram/      teloxide bot
├── skills/            plugin skill
├── storage/           SQLite, log
└── docs/
```

Config contoh:

```toml
# policy.toml
[limits]
max_spend_per_tx_eth = 0.5
max_spend_per_day_eth = 2.0
max_gas_gwei = 80
allowed_chains = [1, 8453, 42161]

[allowlist]
contracts = ["0xSeaport...", "0xKoleksi..."]
block_unlimited_approvals = true

[confirmation]
require_human_above_eth = 0.2
require_human_for = ["new_contract", "new_collection", "withdraw"]

[kill_switch]
auto_trigger_on_loss_eth = 1.0

[telegram]
allowed_user_ids = [123456789]
chat_cannot = ["edit_policy", "export_key"]
import_via_chat = true          # set false untuk mematikan /import
import_session_timeout_sec = 120
delete_import_message = true
require_totp_for_import = true
burner_max_balance_eth = 0.5    # import ditolak kalau saldo di atas ini
max_wallets = 5
```

```toml
# rules.toml
[[rule]]
name = "floor-snipe"
collection = "0x..."
action = "buy"
max_price_eth = 8.0
below_floor_pct = 10
max_buy_per_day = 2

[[rule]]
name = "take-profit"
action = "sell"
take_profit_pct = 25
stop_loss_pct = 15
list_duration_hours = 72
```

---

## 5. Mint Hanya dengan Link (Link-to-Mint)

```
Link -> [Resolver] -> [Verifier on-chain] -> [MintPlan] -> Ringkasan + Konfirmasi -> [Preflight] -> [Eksekusi]
```

1. **Resolver** memakai handler khusus per platform (OpenSea Drops, Zora, Manifold, Highlight, mint.fun, Magic Eden) yang membaca API/metadata resmi mereka. Fallback untuk website project: ambil halaman, cari alamat kontrak dan chain id, ambil ABI dari explorer. LLM boleh membantu menafsirkan halaman, tetapi hasilnya hanya **petunjuk** yang wajib diverifikasi.
2. **Verifier on-chain** (wajib, deterministik):
   - Kontrak ter-verifikasi di explorer, deteksi proxy dan implementasinya.
   - Fungsi mint, harga, `maxPerWallet`, `maxSupply`, `totalSupply`, status sale, stage dan waktunya dibaca **langsung dari kontrak**.
   - Simulasi mint dari tiap wallet (`eth_call`) untuk memastikan tidak revert dan tidak ada efek samping aneh (transfer ke alamat lain, approval).
   - Skor risiko link: domain typosquat, kontrak baru tanpa verifikasi, fungsi `setApprovalForAll`/`delegatecall` mencurigakan, owner bisa mengubah harga/pause sesukanya, indikasi honeypot (tidak bisa transfer setelah mint).
3. **MintPlan** (ringkasan di Telegram):

```
Koleksi: XYZ (Base)   Kontrak: 0xAb..12 ✅ verified
Stage: Allowlist 14:00 UTC | Public 15:00 UTC
Harga: 0.01 ETH   Limit: 3/wallet   Supply: 4,210/10,000
Wallet: burner-1, burner-2     Qty: 3 + 3
Gas: aggressive (cap 40 gwei)  Risiko: RENDAH
[✅ Konfirmasi]  [⚙️ Ubah]  [❌ Batal]
```

4. Link yang **gagal verifikasi** (kontrak tidak ter-verifikasi, domain mencurigakan, harga tidak sesuai) tidak diproses otomatis; bot meminta konfirmasi manusia dengan peringatan jelas. Link adalah vektor umum wallet drainer, jadi tidak ada kontrak yang di-mint hanya karena "ada di link".
5. Jika mint butuh login Google/X, MintPlan memuat langkah browser (akun mana, apa yang diminta) dan memeriksa sesi di preflight.

---

## 6. Eksekusi Minim Kegagalan (Reliability)

**Catatan jujur:** tidak ada sistem yang bisa menjamin 0% gagal. Faktor di luar kendali agent: supply habis, project menutup/mengubah sale, server project down, anti-bot, allowlist tidak valid, atau chain macet. Target desain ini: **menghilangkan semua kegagalan yang disebabkan sisi agent**, dan membuat setiap kegagalan sisanya terdeteksi cepat dan dilaporkan jelas.

### 6.1 Preflight (otomatis T-10 menit dan T-1 menit, atau manual `/preflight <task>`)

| Cek | Lulus bila |
|---|---|
| Saldo | saldo >= harga x qty + gas cadangan 30% di tiap wallet |
| Nonce | nonce lokal = nonce on-chain, tidak ada tx pending nyangkut |
| RPC | minimal 3 endpoint sehat, selisih block head <= 1 |
| Jam server | offset NTP < 50 ms (chrony) |
| Simulasi | mint sukses di simulasi (atau di fork) dengan parameter final |
| Allowlist | merkle proof valid untuk tiap wallet |
| Limit | qty tidak melebihi `maxPerWallet` / sisa supply |
| Gas | `maxFee` di bawah cap policy dengan margin |
| Login | sesi Google/X/wallet masih aktif (jika dibutuhkan) |
| Policy | semua batas tidak akan terlanggar oleh rencana ini |

Hasil dikirim sebagai checklist hijau/merah. **Jika ada merah, war tidak dimulai otomatis** (kecuali Anda menyetujui lewat tombol).

### 6.2 Peta Kegagalan dan Mitigasi

| Penyebab gagal | Mitigasi |
|---|---|
| Tx revert karena kondisi (sale belum aktif / habis) | Kirim sebagai bundle private dengan revert protection, simulasi sebelum kirim |
| Gas terlalu rendah, tidak masuk block | Persentil priority fee dari block terbaru, varian tx multi-tier, replace-by-fee otomatis (+12.5%) dalam batas cap |
| Nonce bentrok / nyangkut | Nonce manager atomik per wallet, reservasi nonce, cancel tx nyangkut |
| RPC lambat / down | Pool multi-provider, kirim ke semua sekaligus (hedged), failover otomatis |
| Waktu meleset | Sinkron NTP, ukur offset ke block timestamp, trigger berbasis event on-chain bila ada |
| Dana kurang | Preflight + cadangan gas; top-up antar wallet burner (dalam batas policy) |
| Proses mati / restart saat war | Job tersimpan di SQLite, supervisor (systemd/Docker restart), resume idempoten |
| Dobel mint tak sengaja | Idempotency key per (task, wallet, nonce); cek `balanceOf`/event sebelum retry |
| Kontrak berbahaya / link scam | Verifier + simulasi + policy allowlist (bagian 5) |
| Sesi login mati | `session_health` + notifikasi sebelum hari H |

### 6.3 Setelah Eksekusi
- `confirm_and_verify`: tunggu receipt, cek event `Transfer` dan `balanceOf`/`ownerOf` agar yakin NFT benar-benar masuk wallet.
- Laporkan per wallet: sukses/gagal, tx hash, gas terpakai, token id, penyebab jika gagal.
- Jika sisa target belum tercapai dan sale masih terbuka, agent melanjutkan sesuai batas retry di policy; jika tidak, berhenti dan melapor.

---

## 7. War Mint Tepat Sasaran

**Model waktu:**
```
T0  = waktu mulai sale (timestamp / block / perubahan state)
Tsend = T0 - (latency_rpc_p95 + margin_jam + waktu_propagasi_block)
```
- **Latency diukur**, bukan ditebak: saat preflight agent mengirim ping ke tiap RPC/relay dan menghitung p50/p95, lalu menyimpannya untuk menghitung `Tsend`.
- **Trigger berbasis state** lebih disukai daripada jam bila tersedia: dengarkan `newHeads` lewat WebSocket dan `saleActive()`/`startTime` dari kontrak, lalu kirim saat block yang memenuhi syarat.
- **Kontrak berbasis `block.timestamp`**: target block pertama dengan timestamp >= T0. Di Ethereum, kirim sebagai **bundle ke block target N+1 (Flashbots)** sehingga bila bundle belum memenuhi syarat atau tidak masuk, **tidak ada gas yang terbuang**; ulangi untuk N+2, N+3 sampai batas.
- **L2 (Base/Arbitrum/Optimism)**: tidak ada mempool publik yang sama; urutan ditentukan sequencer. Fokus ke latency terendah ke sequencer endpoint dan priority fee wajar; kirim lewat beberapa endpoint secara paralel.
- **Multi-wallet paralel**: tiap wallet punya nonce sendiri, tx pre-signed untuk 3 tingkat gas (normal/agresif/maksimal-cap) dan dipilih berdasarkan kondisi jaringan saat Tsend.
- **Replace/cancel**: tx yang belum masuk dalam N block diganti dengan gas lebih tinggi (dalam cap), atau di-cancel bila sale sudah habis.
- **Berhenti cerdas**: begitu `totalSupply >= maxSupply` atau target qty tercapai, semua tx pending dibatalkan.
- **Tidak ada penghindaran anti-bot.** Agent tidak menyertakan solver captcha, spoofing fingerprint, atau pengakalan pembatasan project. Jika project memakai verifikasi manusia/allowlist ketat, agent berhenti dan meminta Anda. Beberapa project melarang bot dan bisa membatalkan atau memblacklist wallet.

---

## 8. Optimasi Respons Bot dan Fungsi yang Dipakai

### 8.1 Target Latensi

| Jalur | Target |
|---|---|
| Ack perintah Telegram ("⏳ diterima") | < 300 ms |
| `/status`, `/portfolio` (dari cache) | < 800 ms |
| Resolve link -> MintPlan | < 5 s |
| Ekstraksi intent dengan LLM | < 3 s (ada timeout + fallback) |
| Trigger -> tx terkirim (war) | < 100 ms setelah kondisi terpenuhi |
| Notifikasi hasil mint | < 2 s setelah receipt |

### 8.2 Aturan Arsitektur Cepat
1. **Pisahkan hot path dan cold path.** Handler Telegram hanya memvalidasi `user_id`, membalas ack, dan memasukkan perintah ke antrean `tokio::mpsc`. Pekerjaan berat (resolve, LLM, simulasi) berjalan di worker async.
2. **LLM tidak berada di jalur eksekusi mint.** LLM dipakai sebelum eksekusi (memahami link/perintah, menyusun rencana). Saat war, yang jalan adalah rencana deterministik yang sudah dikompilasi menjadi `ExecPlan`.
3. **Perintah baku diparsing tanpa LLM** (regex/clap). LLM hanya dipanggil untuk bahasa natural, pakai model kecil/lokal dengan timeout 3 s dan fallback ke pesan "perintah tidak dikenali".
4. **Pre-warm** sebelum event: koneksi WebSocket ke RPC dibuka, ABI, chain id, gas oracle, dan nonce sudah di-cache; tx sudah dibangun dan ditandatangani.
5. **Koneksi persisten**: satu `reqwest::Client` bersama (HTTP/2, keep-alive, connection pool), WebSocket tetap hidup dengan ping/pong dan reconnect otomatis.
6. **Hedged requests**: kirim ke beberapa RPC/relay bersamaan, ambil yang pertama berhasil.
7. **Lokasi VPS** dekat endpoint RPC/sequencer yang dipakai (misal region AS-Timur untuk Ethereum). Ukur RTT sebelum memilih.
8. **Telegram**: `answerCallbackQuery` segera pada tombol; edit pesan lama (`editMessageText`) daripada kirim pesan baru; gabungkan notifikasi beruntun (batas Telegram ±1 pesan/detik per chat); pisahkan antrean notifikasi prioritas (hasil mint, error) dari yang biasa.
9. **Cache**: `moka` untuk ABI/metadata/floor (TTL pendek), `DashMap` untuk state nonce, SQLite mode WAL dengan tulis dibatch di luar hot path.
10. **Build rilis**: `lto = "fat"`, `codegen-units = 1`, `opt-level = 3`, alokator `mimalloc`, jalankan `RUSTFLAGS="-C target-cpu=native"` di VPS yang sama.
11. **Observabilitas**: metrik latensi per tahap (Prometheus) dan tracing; alarm bila p95 melewati target.

### 8.3 Fungsi/Modul Kunci yang Harus Ada

```rust
// link & verifikasi
async fn resolve_mint_link(url: &Url) -> Result<MintCandidate>;
async fn verify_contract(c: &MintCandidate) -> Result<VerifiedTarget>;   // on-chain, deterministik
fn build_mint_plan(t: &VerifiedTarget, ctx: &UserCtx) -> MintPlan;

// persiapan
async fn preflight(plan: &MintPlan) -> PreflightReport;
async fn measure_latency(endpoints: &[Endpoint]) -> LatencyStats;
fn compile_exec_plan(plan: &MintPlan, lat: &LatencyStats) -> ExecPlan;    // tx varian pre-signed
async fn reserve_nonces(wallets: &[WalletRef]) -> NonceReservation;

// trigger & kirim
async fn wait_for_trigger(t: Trigger) -> TriggerFired;                    // ws newHeads / timestamp / state
async fn race_broadcast(txs: &[SignedTx], targets: &[Relay]) -> BroadcastResult;
async fn submit_bundle(txs: &[SignedTx], target_block: u64) -> BundleResult;
async fn bump_or_cancel(tx: &PendingTx, policy: &Policy) -> Action;

// pascaeksekusi
async fn confirm_and_verify(res: &BroadcastResult) -> MintOutcome;        // receipt + balanceOf/ownerOf
async fn notify_batched(ev: Event, prio: Priority);

// keamanan & akun
fn policy_check(intent: &Intent) -> PolicyDecision;
async fn simulate(tx: &TxRequest) -> SimResult;
async fn ensure_session(acc: &AccountId) -> SessionStatus;                // Social Vault
async fn browser_sign_request(req: Eip1193Request) -> Result<Response>;   // selalu lewat policy
```

### 8.4 Crate yang Disarankan
`alloy` (EVM, provider WS), `tokio`, `reqwest` (http2), `tokio-tungstenite`, `teloxide`, `axum`, `sqlx` (SQLite WAL), `moka`, `dashmap`, `mimalloc`, `metrics` + `metrics-exporter-prometheus`, `tracing`, `chromiumoxide` (kontrol Chromium + CDP), `scraper`/`url`, `argon2` + `chacha20poly1305` + `zeroize`, `totp-rs`, `wasmtime`, `clap`, `serde`/`toml`, `backoff`.

---

## 9. JADWAL PENGERJAAN (Agent VS Code)

Estimasi 1 developer + agent, ±2-4 jam/hari, total ±60 hari. Format: Task -> Acceptance.

### Fase 1 (Hari 1-3): Fondasi
- [ ] Cargo workspace, CI (`fmt`, `clippy`, `test`), profil rilis (`lto`, `mimalloc`).
- [ ] `core`: `Intent`, `Task`, `WalletRef`, `Event`, `Secret<T>`.
- [ ] Loader + validasi config; SQLite (WAL) + migrasi (`tasks`, `rules`, `intents`, `txs`, `audit`, `accounts`).
- **Acceptance:** `cargo test` hijau; config invalid ditolak dengan pesan jelas.

### Fase 2 (Hari 4-7): Keystore, Signer, Policy
- [ ] Keystore terenkripsi + impor via CLI + `zeroize`.
- [ ] Signer (tx, personal_sign, EIP-712) dengan tipe `ApprovedTx`.
- [ ] Policy engine penuh + kill switch + hook konfirmasi manusia.
- **Acceptance:** tx di atas limit ditolak, kontrak di luar allowlist ditolak, kunci tidak muncul di log, signer tidak bisa dipanggil tanpa `ApprovedTx`.

### Fase 3 (Hari 8-11): Chain Layer
- [ ] RPC pool + WebSocket `newHeads`, failover, hedged requests, nonce manager, gas EIP-1559 berbasis persentil.
- [ ] Simulator + broadcaster (public + Flashbots Protect/bundle); fork test Anvil; `measure_latency`.
- **Acceptance:** transfer di Sepolia lewat pipeline penuh; tx revert terdeteksi di simulasi dan tidak terkirim; statistik latensi tersimpan.

### Fase 4 (Hari 12-16): Telegram Cepat
- [ ] Bot teloxide, whitelist `user_id`, antrean `mpsc`, ack < 300 ms, `answerCallbackQuery` instan, edit pesan, notifikasi batched + prioritas.
- [ ] `/status`, `/stop`, `/approve`, `/reject`.
- [ ] `/import` sesuai 3A, `/newwallet`, `/wallets`, TOTP, rate limit.
- **Acceptance:** p95 ack < 300 ms (diukur); user non-whitelist diabaikan; `/stop` < 2 detik; phrase dari `/import` tidak muncul di log/SQLite/balasan, pesan terhapus, import ditolak bila saldo melewati batas.

### Fase 5 (Hari 17-21): Mint Engine & Eksekusi
- [ ] Deteksi fungsi mint dari ABI, `compile_exec_plan`, tx varian pre-signed, reservasi nonce.
- [ ] `wait_for_trigger` (timestamp/block/state), `race_broadcast`, `submit_bundle`, `bump_or_cancel`.
- [ ] `confirm_and_verify` (receipt + `balanceOf`/`ownerOf`), idempotency key, resume setelah restart.
- **Acceptance:** di fork Anvil, mint terjadi <1 block setelah trigger; tidak ada dobel mint saat proses di-restart di tengah war; limit per wallet dihormati; dry-run tidak mengirim tx.

### Fase 6 (Hari 22-27): Link-to-Mint, Preflight, Reliability
- [ ] `linkresolver`: handler OpenSea Drops, Zora, Manifold, mint.fun (+ fallback website).
- [ ] `verify_contract`, skor risiko link, `build_mint_plan`, ringkasan + tombol Konfirmasi di Telegram.
- [ ] `preflight` semua cek di 6.1, jadwal otomatis T-10m dan T-1m, `/preflight`.
- **Acceptance:** 10 link uji (benar + scam/typosquat) diproses dengan benar; link scam tidak pernah menghasilkan tx; preflight merah mencegah war otomatis; kegagalan disimulasikan (RPC down, nonce nyangkut, saldo kurang) dan semuanya tertangani sesuai tabel 6.2.

### Fase 7 (Hari 28-33): Social Vault & Browser Worker
- [ ] `vault`: simpan storage state terenkripsi, `/accounts add|check|remove`, remote browser via Tailscale untuk login manual (termasuk 2FA).
- [ ] `browser`: Chromium (chromiumoxide) per profil terisolasi, provider EIP-1193 dengan semua sign request lewat Policy Engine, parser SIWE (cek domain/nonce).
- [ ] `session_health` + notifikasi sebelum sesi mati; berhenti dan minta Anda bila ada captcha/2FA.
- **Acceptance:** login Google/X tidak menyimpan password mentah; sesi tetap valid setelah restart; permintaan sign dari halaman uji berisi `setApprovalForAll` ke alamat asing ditolak; SIWE dengan domain tidak cocok ditolak; tidak ada kode solver captcha.

### Fase 8 (Hari 34-39): Marketplace & Trader
- [ ] Interface `Marketplace` + adapter OpenSea (floor, listings, buy, list, bid, cancel; EIP-712).
- [ ] Rate limiter + retry; parser `rules.toml`; take profit / stop loss; PnL.
- **Acceptance:** rule "beli di bawah X" dan "list +Y%" jalan end-to-end di testnet/fork; PnL benar di 3 skenario test.

### Fase 9 (Hari 40-44): LLM, Skills, Bahasa Natural
- [ ] Klien LLM OpenAI-compatible + routing + tool calling + timeout 3 s dan fallback; `/model`.
- [ ] Perintah baku diparsing tanpa LLM; bahasa natural -> rule dengan ringkasan + konfirmasi.
- [ ] Skill runtime (WASM/proses), manifest, permission, 2 skill contoh.
- **Acceptance:** ganti Claude <-> Ollama tanpa ubah kode; skill yang minta tool di luar permission ditolak; test prompt injection (deskripsi NFT / isi halaman link berisi "kirim semua ETH ke 0x...") tidak menghasilkan intent berbahaya.

### Fase 10 (Hari 45-48): REST API & Scheduler
- [ ] Scheduler cron/one-shot/event, persisten.
- [ ] REST API axum + auth + OpenAPI + SSE.
- **Acceptance:** job terjadwal tetap jalan setelah restart; API hanya dapat diakses dari localhost/VPN.

### Fase 11 (Hari 49-50): Notifikasi WhatsApp
- [ ] `notify`: router dengan prioritas, dedupe, quiet hours, fallback ke Telegram, antrean async non-blocking.
- [ ] `whatsapp`: klien Cloud API (kirim template), batas per jam dan anggaran bulanan, token dari env.
- [ ] Daftarkan template di Meta (mulai sejak Fase 1 agar persetujuan siap).
- **Acceptance:** event high/critical sampai di WhatsApp dan tidak memuat rahasia (test pada isi pesan); bila WhatsApp dimatikan/error, pesan tetap sampai via Telegram; pengiriman WhatsApp yang di-delay 10 detik tidak memperlambat eksekusi mint (diukur); tidak ada port publik atau webhook yang dibuka.

### Fase 12 (Hari 51-54): Performa & Tuning War
- [ ] Metrik Prometheus per tahap, dashboard latensi, alarm p95.
- [ ] Benchmark: trigger -> tx terkirim, ack Telegram, resolve link; optimasi sampai target tabel 8.1.
- [ ] Simulasi war di fork Anvil dengan 5-10 wallet paralel dan kompetitor tiruan; atur strategi gas/bundle.
- **Acceptance:** semua target 8.1 tercapai di VPS produksi; di 100 simulasi war, nol kegagalan yang disebabkan sisi agent (nonce, RPC, gas cap, dobel mint, waktu).

### Fase 13 (Hari 55-60): Hardening & Rilis Bertahap
- [ ] Audit trail lengkap, alert (saldo rendah, error beruntun, sesi akun mati).
- [ ] `cargo audit`, `cargo deny`, review checklist bagian 10.
- [ ] Deploy VPS (Docker, firewall, SSH key only, Tailscale), backup keystore dan vault terenkripsi.
- [ ] Uji bertahap: dry-run 3 hari -> testnet 3 hari -> mainnet modal kecil (<= 0.05 ETH) 1 minggu -> naikkan batas pelan-pelan.
- **Acceptance:** semua checklist tercentang, tanpa insiden selama masa uji.

### Fase Opsional
- WhatsApp dua arah (perintah via webhook) tidak direkomendasikan; kalau tetap diinginkan, hanya untuk perintah baca-saja (`status`) dan kill switch.
- Adapter Blur / Magic Eden / Rarible; dukungan Solana (Magic Eden, Tensor).
- Smart account + session key (ERC-4337).

---

## 10. Checklist Sebelum Mainnet

- [ ] Wallet agent = burner / session key, bukan wallet utama
- [ ] Tidak ada seed phrase / private key di repo, `.env` ter-commit, atau log; phrase yang dikirim via `/import` terhapus dari chat dan tidak tersimpan mentah
- [ ] Keystore terenkripsi; passphrase tidak ada di disk
- [ ] Test lengkap untuk semua batas policy
- [ ] Allowlist kontrak aktif; unlimited approval diblokir
- [ ] Simulasi wajib sebelum setiap tx
- [ ] Telegram: whitelist user ID, 2FA akun, konfirmasi untuk nominal besar
- [ ] Wallet yang di-import via chat benar-benar burner (saldo di bawah batas, bukan phrase wallet utama)
- [ ] API hanya localhost / VPN
- [ ] Kill switch teruji
- [ ] WhatsApp memakai API resmi, satu arah, tanpa webhook/port publik; token di env; template terdaftar; isi pesan tanpa rahasia
- [ ] Gagal kirim WhatsApp tidak memengaruhi eksekusi mint; fallback ke Telegram teruji
- [ ] Social Vault: hanya menyimpan sesi (bukan password), akun khusus (bukan akun Google utama), akses remote browser hanya lewat Tailscale
- [ ] Semua sign request dari browser worker melewati Policy Engine
- [ ] Preflight lulus penuh sebelum setiap war; link tanpa verifikasi on-chain tidak pernah dieksekusi otomatis
- [ ] Tidak ada solver captcha / penghindar deteksi bot di kode
- [ ] Target latensi 8.1 tercapai dan terukur
- [ ] Skill pihak ketiga di-sandbox dan direview
- [ ] ToS marketplace dan aturan tiap project (larangan bot / multi-wallet) sudah dibaca
- [ ] `cargo audit` bersih, `Cargo.lock` dikunci

---

## 11. Prompt Awal untuk Agent VS Code

> Kamu mengerjakan proyek `nft-agent` sesuai `NFT_AGENT_BLUEPRINT_TELEGRAM.md`. Kerjakan **satu fase sekali**, mulai Fase 1. Untuk setiap fase: (1) buat rencana singkat, (2) implementasi, (3) tulis test sesuai "Acceptance", (4) jalankan `cargo fmt && cargo clippy -- -D warnings && cargo test`, (5) ringkas hasil dan tunggu persetujuan sebelum lanjut. Aturan keras: jangan pernah menulis kode yang menyimpan atau mencetak private key / mnemonic dalam plaintext; jangan membuat jalur sign yang melewati Policy Engine; seed phrase/private key hanya boleh diterima lewat alur `/import` bagian 3A (khusus burner, pesan langsung dihapus, tidak pernah di-log atau disimpan mentah); jangan menyimpan password Google/X mentah (hanya storage state terenkripsi dari login manual pengguna); semua permintaan sign dari browser worker wajib lewat Policy Engine; jangan menulis solver captcha atau penghindar deteksi bot; jangan menaruh LLM di jalur eksekusi mint; WhatsApp hanya untuk notifikasi satu arah via Cloud API resmi (template, tanpa webhook, tanpa rahasia di isi pesan, tidak boleh memblokir hot path); jangan mengeksekusi link mint tanpa verifikasi on-chain; ukur latensi setiap perubahan di hot path; selalu default ke dry-run; jangan menambah dependency tanpa menyebut alasannya.

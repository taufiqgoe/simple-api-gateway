# Panduan Operasi simple-api-gateway

Panduan lengkap menjalankan, mengonfigurasi, men-deploy, dan memecahkan masalah gateway ini.
Ringkasan ada di [README.md](README.md).

## 1. Gambaran singkat

Gateway menerima HTTP/1.1 di satu port, mencocokkan path dengan daftar rute (longest prefix),
memilih satu upstream sesuai persentase weight, lalu meneruskan request dan men-stream respons kembali.

Fitur operasional (sejak 0.2.0): health check aktif + pasif, retry otomatis saat gagal connect, hot reload (SIGHUP),
metrics Prometheus, dan access log opsional. Batasan yang disengaja (tidak ada): TLS, auth, rate limiting,
WebSocket/Upgrade, HTTP/2. Lihat bagian 9 dan 11.

## 2. Menjalankan

### 2.1 Langsung (tanpa Docker)

Prasyarat: Rust (instal via https://rustup.rs).

```bash
cargo build --release
GATEWAY_CONFIG=./config.yaml ./target/release/simple-api-gateway
```

Binary hasil build: `target/release/simple-api-gateway` (satu file, tanpa dependensi runtime).

### 2.2 Docker Compose (paling cepat untuk mencoba)

```bash
docker compose up --build -d
```

Menjalankan gateway di `localhost:3000` beserta dua upstream dummy (`app-v1`, `app-v2`).
Konfigurasi dibaca dari `./config.yaml` (di-mount read-only).

```bash
# ~80 app-v1 / ~20 app-v2
for i in $(seq 100); do curl -s localhost:3000/api/x; done | sort | uniq -c
# selalu app-v2 (rute lebih spesifik)
curl localhost:3000/api/something/1
docker compose down
```

### 2.3 Docker saja

```bash
docker build --platform linux/amd64 -t simple-api-gateway .
docker run -d --name gateway -p 3000:3000 \
  -v $(pwd)/config.yaml:/etc/gateway/config.yaml:ro \
  simple-api-gateway
```

- Image `scratch`, binary statis (musl), berjalan sebagai UID/GID 65534 (non-root), ukuran ~2 MB.
- Config **tidak** di-bake ke image; wajib di-mount ke `/etc/gateway/config.yaml`
  (atau mount ke path lain dan set `-e GATEWAY_CONFIG=/path`).
- Jika `listen` diubah dari port 3000, sesuaikan `-p`. `EXPOSE 3000` hanya dokumentasi.
- Port < 1024 pada container non-root: di Docker biasanya tetap bisa karena port dipetakan di sisi host
  (`-p 80:3000`), jadi cukup pertahankan `listen` di port tinggi.
- Upstream harus bisa dijangkau dari container. `localhost` di config berarti container itu sendiri;
  gunakan nama service (Compose/jaringan Docker yang sama), IP, atau `host.docker.internal` (Docker Desktop).

### 2.4 Environment variable

| Variabel | Default | Fungsi |
|---|---|---|
| `GATEWAY_CONFIG` | `/etc/gateway/config.yaml` | Path file konfigurasi |

## 3. Konfigurasi

Satu file YAML, dibaca **sekali** saat startup. Mengubah konfigurasi = restart proses/container
(`docker compose restart gateway`).

```yaml
listen: "0.0.0.0:3000"
timeout_secs: 30

routes:
  - path: /api/something
    upstreams:
      - url: http://localhost:8081
        weight: 100

  - path: /api
    strip_prefix: false
    upstreams:
      - url: http://localhost:8080
        weight: 80
      - url: http://localhost:8082
        weight: 20
```

### 3.1 Referensi field

| Field | Wajib | Default | Keterangan |
|---|---|---|---|
| `listen` | tidak | `0.0.0.0:3000` | Alamat `IP:port`. Hostname tidak didukung, gunakan IP. |
| `timeout_secs` | tidak | `30` | Harus > 0. Batas waktu menunggu **header respons** upstream (termasuk connect). Juga batas tunggu graceful shutdown. |
| `routes` | ya | – | Minimal satu rute. |
| `routes[].path` | ya | – | Harus diawali `/`. Trailing slash diabaikan (`/api/` ≡ `/api`). `/` cocok dengan semua path. Tidak boleh duplikat. |
| `routes[].strip_prefix` | tidak | `false` | `true` = buang prefix rute sebelum diteruskan. |
| `routes[].upstreams` | ya | – | Minimal satu. |
| `upstreams[].url` | ya | – | Hanya `http://host[:port]`. Tanpa path, query, atau `https://`. |
| `upstreams[].weight` | ya | – | Integer 1–100. **Total per rute harus tepat 100.** |

Field yang tidak dikenal (termasuk salah ketik) membuat gateway menolak start dengan pesan yang
menyebut baris dan kolom.

### 3.2 Aturan pencocokan rute

1. **Longest prefix match**: `/api/something/123` masuk ke rute `/api/something`, bukan `/api`.
2. **Batas segmen**: `/api` cocok dengan `/api` dan `/api/x`, **tidak** dengan `/apix`.
   `/api/somethingelse` jatuh ke `/api`, bukan `/api/something`.
3. Pencocokan hanya pada path; query string tidak ikut dicocokkan.
4. Tidak ada rute yang cocok → `404`.
5. Urutan penulisan rute di YAML tidak berpengaruh.
6. Path tidak dinormalisasi (`/api/../x`, percent-encoding diteruskan apa adanya).

### 3.3 `strip_prefix`

Untuk rute `/strip` dengan `strip_prefix: true`:

| Request klien | Diterima upstream |
|---|---|
| `/strip/foo/bar?a=1` | `/foo/bar?a=1` |
| `/strip` | `/` |
| `/strip?q=2` | `/?q=2` |

Dengan `false`, path dan query diteruskan tanpa perubahan (`/api/x` tetap `/api/x`).

### 3.4 Load balancing

- Saat startup dibangun tabel 100 slot per rute (slot tiap upstream tersebar merata, tidak berurutan).
  Setiap request mengambil slot berikutnya lewat `AtomicUsize::fetch_add` modulo 100 (tanpa lock).
- Konsekuensi: distribusi **tepat** sesuai persentase tiap 100 request pada satu rute
  (80/20 → 80 dan 20), bukan acak.
- Counter per rute, bersama seluruh koneksi. Rute berupstream tunggal tidak memakai counter.
- Tidak ada health check atau failover: upstream yang mati tetap mendapat bagiannya
  dan request ke sana dijawab `502`. Hapus/ubah weight lewat edit config + restart.

### 3.5 Contoh pola umum

Canary 95/5:

```yaml
routes:
  - path: /
    upstreams:
      - { url: "http://app-stable:8080", weight: 95 }
      - { url: "http://app-canary:8080", weight: 5 }
```

Beberapa layanan dengan prefix dibuang:

```yaml
routes:
  - path: /users
    strip_prefix: true      # /users/42 -> /42
    upstreams: [{ url: "http://users:8080", weight: 100 }]
  - path: /orders
    strip_prefix: true
    upstreams: [{ url: "http://orders:8080", weight: 100 }]
  - path: /
    upstreams: [{ url: "http://web:8080", weight: 100 }]   # fallback semua sisanya
```

## 4. Perilaku proxy

**Request ke upstream**
- `Host` diganti dengan host:port upstream.
- `X-Forwarded-For` ditambah IP koneksi klien. Jika header sudah ada, nilainya dipertahankan dan IP
  ditambahkan di belakangnya (`1.2.3.4, 10.0.0.5`).
- Header hop-by-hop dibuang di kedua arah: `Connection`, `Keep-Alive`, `Proxy-Authenticate`,
  `Proxy-Authorization`, `Proxy-Connection`, `TE`, `Trailer`, `Transfer-Encoding`, `Upgrade`,
  beserta header apa pun yang disebut di dalam `Connection: ...`.
- Request ke upstream memakai HTTP/1.1 dengan connection pooling (koneksi di-reuse).
- Body request dan respons di-stream, tidak ditampung penuh di memori.

**Kode status buatan gateway**

| Kode | Kondisi |
|---|---|
| `404` | Tidak ada rute cocok |
| `502` | Upstream tidak bisa dihubungi / koneksi gagal / error protokol |
| `504` | Upstream tidak mengirim header respons dalam `timeout_secs` |
| `400` | Path hasil `strip_prefix` tidak valid (jarang) |

Timeout hanya mencakup sampai header respons. Body respons yang panjang (download besar) tidak
dibatasi `timeout_secs`.

**Penting soal `X-Forwarded-For`**: nilai dari klien dipercaya dan ditambahi. Jika gateway menghadap
internet langsung, klien bisa memalsukan awal rantai itu; upstream sebaiknya hanya percaya entri
paling kanan.

## 5. Log

Hanya ke stdout, tanpa access log:

```
gateway listening on 0.0.0.0:3000 (2 routes)
error: upstream app-v1:8080 gagal: client error (Connect)
error: upstream app-v1:8080 timeout
error: accept: ...
shutting down...
```

Lihat log container: `docker logs gateway` / `docker compose logs -f gateway`.
Untuk melihat trafik per request, gunakan log upstream atau load balancer di depan.

## 6. Menghentikan (graceful shutdown)

`SIGTERM` atau `SIGINT` (Ctrl+C):
1. Berhenti menerima koneksi baru.
2. Koneksi idle ditutup; request yang sedang berjalan diselesaikan.
3. Menunggu paling lama `timeout_secs`, lalu keluar.

`docker stop` mengirim SIGTERM (gateway PID 1 menanganinya). Jika `timeout_secs` > 10 dan Anda ingin
menunggu penuh, tambahkan `docker stop -t <detik>` atau `stop_grace_period` di Compose.

## 7. Kesalahan saat startup

Gateway keluar dengan **exit code 1** dan pesan `error: ...` jika:

| Pesan (ringkas) | Penyebab / solusi |
|---|---|
| `tidak bisa membaca <path>` | File config tidak ada / tidak ter-mount. Cek volume dan `GATEWAY_CONFIG`. |
| `unknown field \`x\` ... at line N column M` | Salah ketik nama field. Perbaiki di lokasi yang disebut. |
| `total weight harus 100, ditemukan N` | Jumlah weight satu rute ≠ 100. |
| `weight N di luar 1..=100` | Weight 0 atau > 100. |
| `path 'x' harus diawali '/'` / `duplikat` | Perbaiki path rute. |
| `hanya skema http:// yang didukung` | Upstream `https://` tidak didukung. |
| `tidak boleh berisi path atau query` | Tulis upstream hanya `http://host:port`. |
| `tidak bisa listen di ...` | Port dipakai proses lain / alamat tidak valid / tanpa izin. |

Contoh:

```
error: config /etc/gateway/config.yaml: routes[1] (/api): total weight harus 100, ditemukan 110
```

## 8. Pemecahan masalah

| Gejala | Pemeriksaan |
|---|---|
| Selalu `404` | Path tidak diawali prefix rute pada batas segmen (`/apix` ≠ `/api`). Cek rute di config. |
| `502` pada sebagian request | Satu upstream mati; cek log `error: upstream X gagal`. Bagian request ke upstream itu sesuai weight-nya. |
| `502` semua, di dalam Docker, upstream `localhost:...` | `localhost` = container gateway. Pakai nama service atau `host.docker.internal`. |
| `504` | Upstream lambat membalas header. Perbaiki upstream atau naikkan `timeout_secs`. |
| Upstream menerima path yang tak diharapkan | Periksa `strip_prefix` rute tersebut. |
| Perubahan config tidak berlaku | Tidak ada hot reload; restart gateway. |
| Container Mac M1/M2 gagal build | Gunakan `--platform linux/amd64` (sudah di Compose). |
| WebSocket/SSE gagal | WebSocket tidak didukung (`Upgrade` dibuang). SSE biasa (streaming) berfungsi selama header tiba dalam `timeout_secs`. |

Uji cepat tanpa Docker:

```bash
python3 -m http.server 8080 &                       # upstream dummy
GATEWAY_CONFIG=./config.local.yaml ./target/release/simple-api-gateway &
curl -i localhost:3000/api/
```

## 9. Di luar cakupan

Auth, rate limiting, TLS, health check, service discovery, hot reload, metrics, admin API,
WebSocket, HTTP/2, dan access log sengaja tidak ada. Untuk TLS, taruh gateway di belakang
terminator TLS/load balancer (lalu perhatikan catatan `X-Forwarded-For` di atas).

## 11. Health check, retry, reload, metrics (0.2.0)

```yaml
admin_listen: "0.0.0.0:3001"   # opsional; tanpa ini tidak ada listener admin
access_log: false              # true = satu baris stdout per request
health:                        # semua opsional (nilai default ditampilkan)
  interval_secs: 2             # jeda probe aktif
  probe_timeout_secs: 2
  fail_threshold: 2            # probe gagal / timeout berurutan sebelum dikeluarkan
  eject_secs: 10               # lama dikeluarkan setelah gagal connect / timeout berulang
  retry_body_limit_bytes: 262144
routes:
  - path: /
    health_path: /actuator/health   # opsional: probe aktif GET, sukses = 2xx
    upstreams: [...]
```

- **Pasif (selalu aktif):** gagal *connect* ke upstream → langsung dikeluarkan `eject_secs`, lalu dicoba lagi (half-open).
  Timeout menunggu header → dikeluarkan setelah `fail_threshold` kali berturut-turut. Respons upstream (termasuk 5xx) tidak dihitung gagal.
- **Aktif:** hanya rute dengan `health_path`; `fail_threshold` probe gagal → keluar, satu probe sukses → masuk lagi.
- **Pembagian saat ada yang keluar:** porsinya dibagi ke upstream sehat (lewat tabel slot). Bila semua keluar, gateway tetap mencoba (fail-open). Upstream tunggal tidak pernah dikeluarkan dari rute.
- **Retry:** hanya untuk gagal *connect* (belum ada byte terkirim, jadi aman untuk POST), satu kali, ke upstream lain yang tersedia.
  Body request ≤ `retry_body_limit_bytes` (dengan Content-Length) disangga di memori supaya bisa diulang; body lebih besar atau chunked di-stream tanpa retry.
  Timeout dan error lain TIDAK diulang (request mungkin sudah diproses).
- **Reload:** ubah `config.yaml` lalu `docker kill -s HUP <container>`. Config salah → ditolak, yang lama tetap dipakai
  (`error: reload gagal...` di log). `listen`/`admin_listen` tidak berubah tanpa restart. Counter metrics ter-reset saat reload (Prometheus menangani reset).
- **Admin** (`admin_listen`): `GET /healthz`, `GET /upstreams` (up/down tiap upstream), `GET /metrics` (Prometheus:
  `gateway_requests_total`, `gateway_responses_total{class}`, `gateway_upstream_errors_total{kind}`, `gateway_retries_total`,
  `gateway_ejections_total`, `gateway_upstream_up`, `gateway_request_duration_seconds` (histogram sampai header respons),
  `gateway_no_route_total`, `gateway_config_reloads_total`). Jangan publish ke jaringan umum.

## 12. TLS (HTTPS) sebagai listener tambahan

```yaml
listen: "0.0.0.0:3000"          # tetap HTTP biasa
tls:
  listen: "0.0.0.0:3443"        # HTTPS, dilayani bersamaan dengan listen
  cert: /etc/gateway/tls/fullchain.pem   # sertifikat server + intermediate (server dulu)
  key: /etc/gateway/tls/privkey.pem      # PKCS#1 / PKCS#8 / SEC1
```

- Opsional. Tanpa blok `tls` hanya ada HTTP di `listen`. Dengan blok `tls`, HTTP (`listen`) dan HTTPS (`tls.listen`) jalan
  bersamaan, rute dan upstream sama, satu set metrics. `tls.listen` tidak boleh sama dengan `listen`/`admin_listen`.
- Hanya TLS termination di sisi klien (TLS 1.2/1.3, ALPN `http/1.1`). Koneksi ke upstream tetap `http://`; tanpa mTLS.
- Membuat fullchain dari file CA: `cat server.crt intermediate.crt > fullchain.pem`. Cert dan key harus berpasangan, kalau tidak gateway menolak start (exit 1).
- Perpanjang sertifikat: ganti file di tempat yang sama lalu `docker kill -s HUP <container>`; koneksi baru memakai sertifikat baru.
  Sertifikat/key rusak saat reload ditolak dan sertifikat lama tetap dipakai.
  Mengubah `tls.listen` atau mengaktifkan/menonaktifkan `tls` butuh restart.
- Handshake dibatasi 10 detik. Upstream tidak tahu skema asli (tidak ada `X-Forwarded-Proto`).
- Docker: petakan port, mis. `"443:3443"` untuk HTTPS dan `"48020:3000"` untuk HTTP. Mount cert read-only dan pastikan terbaca UID 65534.
  **Jangan** commit atau bake key ke image (`*.key`, `*.pem`, `*.crt` ada di `.gitignore`).

## 10. Pengembangan

```bash
cargo test                   # 20 unit test: config, longest prefix, batas segmen, strip_prefix, distribusi weight
cargo build --release        # profil: opt-level 3, LTO fat, panic=abort, strip
```

Struktur: `src/main.rs` (server + proxy), `src/config.rs` (parsing & validasi),
`src/router.rs` (pencocokan rute + tabel weight).

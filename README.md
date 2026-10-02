# simple-api-gateway

Panduan lengkap operasi: [OPERASI.md](OPERASI.md).

API gateway minimal dengan Rust (tokio + hyper 1.x): reverse proxy, routing path prefix (longest match),
dan load balancing berdasarkan persentase. Tanpa fitur lain.

## Build & test

```bash
cargo build --release
cargo test
```

## Menjalankan

```bash
GATEWAY_CONFIG=./config.yaml ./target/release/simple-api-gateway
```

`GATEWAY_CONFIG` default `/etc/gateway/config.yaml`. Config dibaca sekali saat startup; config salah → exit 1 dengan pesan error.

### Docker

```bash
docker compose up --build        # gateway di :3000 + 2 upstream dummy
for i in $(seq 100); do curl -s localhost:3000/api/x; done | sort | uniq -c   # ~80 app-v1 / ~20 app-v2
curl localhost:3000/api/something                                              # selalu app-v2
```

Di Apple Silicon image dibuild untuk `linux/amd64` (target `x86_64-unknown-linux-musl`).
Image akhir `scratch`, binary statis, jalan sebagai UID 65534, config di-mount ke `/etc/gateway/config.yaml`.

## Konfigurasi

```yaml
listen: "0.0.0.0:3000"   # default 0.0.0.0:3000
timeout_secs: 30         # default 30; timeout sampai header respons upstream

routes:
  - path: /api/something
    upstreams:
      - url: http://localhost:8081
        weight: 100

  - path: /api
    strip_prefix: false  # default false; true = buang prefix sebelum diteruskan
    upstreams:
      - url: http://localhost:8080
        weight: 80
      - url: http://localhost:8082
        weight: 20
```

- Field tak dikenal ditolak saat startup.
- Total `weight` per rute harus tepat 100 (tiap weight 1–100).
- `url` upstream: hanya `http://host:port` (tanpa path/query, tanpa TLS).
- Pencocokan: longest prefix pada batas segmen (`/api` cocok `/api` dan `/api/x`, bukan `/apix`). `path: /` cocok semua.
- Load balancing: tabel 100 slot (smooth weighted round-robin, terinterleave) + `AtomicUsize::fetch_add`, tanpa lock.
- Header: hop-by-hop dibuang, `X-Forwarded-For` ditambahkan, `Host` diset ke upstream.
- Error: `404` tak ada rute, `502` upstream gagal, `504` timeout.

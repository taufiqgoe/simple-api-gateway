mod config;
mod metrics;
mod router;
mod tls;

use http_body_util::{BodyExt, Either, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::body::Body as _;
use hyper::header::{HeaderMap, HeaderName, HeaderValue, CONNECTION, CONTENT_LENGTH, HOST};
use hyper::http::uri::{PathAndQuery, Scheme};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode, Uri, Version};
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::graceful::GracefulShutdown;
use router::Router;
use std::convert::Infallible;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::time::timeout;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

type Body = Either<Incoming, Full<Bytes>>;

/// Satu generasi konfigurasi. Reload (SIGHUP) membuat generasi baru dan menandai yang lama `retired`.
struct Shared {
    router: Router,
    timeout: Duration,
    access_log: bool,
    health: config::HealthConfig,
    retired: AtomicBool,
}

struct App {
    current: RwLock<Arc<Shared>>,
    /// Acceptor TLS aktif; di-swap saat reload agar sertifikat baru dipakai koneksi berikutnya.
    tls: RwLock<Option<tokio_rustls::TlsAcceptor>>,
    client: Client<HttpConnector, Body>,
    global: metrics::Global,
}

impl App {
    fn shared(&self) -> Arc<Shared> {
        self.current.read().unwrap().clone()
    }
}

/// Body request ke upstream: disangga (bisa diulang) atau di-stream (sekali pakai).
enum Payload {
    Replay(Bytes),
    Stream(Option<Incoming>),
}

const HOP_BY_HOP: [HeaderName; 8] = [
    CONNECTION,
    HeaderName::from_static("keep-alive"),
    HeaderName::from_static("proxy-authenticate"),
    HeaderName::from_static("proxy-authorization"),
    HeaderName::from_static("proxy-connection"),
    hyper::header::TE,
    hyper::header::TRAILER,
    hyper::header::TRANSFER_ENCODING,
];
static X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");

fn strip_hop_by_hop(h: &mut HeaderMap) {
    // Header tambahan yang dinamai di dalam "Connection: a, b" juga hop-by-hop.
    let mut extra: Vec<HeaderName> = Vec::new();
    for v in h.get_all(CONNECTION) {
        for tok in v.as_bytes().split(|&b| b == b',') {
            let tok = tok.trim_ascii();
            if tok.is_empty() || tok.eq_ignore_ascii_case(b"keep-alive") || tok.eq_ignore_ascii_case(b"close") {
                continue;
            }
            if let Ok(n) = HeaderName::from_bytes(tok) {
                extra.push(n);
            }
        }
    }
    for n in HOP_BY_HOP.iter().chain(extra.iter()) {
        h.remove(n);
    }
    h.remove(hyper::header::UPGRADE);
}

fn status(code: StatusCode, msg: &'static str) -> Response<Body> {
    let mut r = Response::new(Either::Right(Full::new(Bytes::from_static(msg.as_bytes()))));
    *r.status_mut() = code;
    r
}

fn add_forwarded_for(h: &mut HeaderMap, peer: IpAddr) {
    let mut s = String::new();
    for v in h.get_all(&X_FORWARDED_FOR) {
        if let Ok(v) = v.to_str() {
            s.push_str(v);
            s.push_str(", ");
        }
    }
    use std::fmt::Write;
    let _ = write!(s, "{peer}");
    if let Ok(v) = HeaderValue::from_str(&s) {
        h.insert(X_FORWARDED_FOR.clone(), v);
    }
}

async fn proxy(app: Arc<App>, peer: IpAddr, tls: bool, req: Request<Incoming>) -> Result<Response<Body>, Infallible> {
    let started = Instant::now();
    let shared = app.shared();
    let host = req.headers().get(HOST).and_then(|v| v.to_str().ok()).or_else(|| req.uri().authority().map(|a| a.as_str()));
    let Some((route, fwd_path)) = shared.router.find(host, tls, req.uri().path()) else {
        app.global.no_route.fetch_add(1, Relaxed);
        return Ok(status(StatusCode::NOT_FOUND, "Not Found\n"));
    };

    // Tanpa strip_prefix, path+query asli dipakai ulang (tanpa alokasi).
    let pq = if route.strip_prefix {
        let s = match req.uri().query() {
            Some(q) => format!("{fwd_path}?{q}"),
            None => fwd_path.to_owned(),
        };
        match s.parse::<PathAndQuery>() {
            Ok(pq) => pq,
            Err(_) => return Ok(status(StatusCode::BAD_REQUEST, "Bad Request\n")),
        }
    } else {
        req.uri().path_and_query().cloned().unwrap_or_else(|| PathAndQuery::from_static("/"))
    };

    let (mut parts, body) = req.into_parts();
    strip_hop_by_hop(&mut parts.headers);
    add_forwarded_for(&mut parts.headers, peer);

    // Body kecil (atau kosong) disangga: kegagalan connect ke satu upstream bisa diulang ke upstream lain tanpa
    // risiko request ganda, karena belum ada byte yang terkirim. Body besar/tak diketahui ukurannya di-stream.
    let limit = shared.health.retry_body_limit_bytes;
    let declared = parts.headers.get(CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
    let bufferable = match declared {
        Some(n) => n <= limit,
        None => body.is_end_stream(),
    };
    let mut payload = if bufferable {
        match timeout(shared.timeout, Limited::new(body, limit as usize).collect()).await {
            Ok(Ok(collected)) => Payload::Replay(collected.to_bytes()),
            Ok(Err(_)) => return Ok(status(StatusCode::BAD_REQUEST, "Bad Request\n")),
            Err(_) => return Ok(status(StatusCode::REQUEST_TIMEOUT, "Request Timeout\n")),
        }
    } else {
        Payload::Stream(Some(body))
    };
    let replayable = matches!(payload, Payload::Replay(_));

    let mut index = route.pick();
    for attempt in 0..2 {
        let up = route.upstream(index);
        up.stats.requests.fetch_add(1, Relaxed);

        let mut uri_parts = hyper::http::uri::Parts::default();
        uri_parts.scheme = Some(Scheme::HTTP);
        uri_parts.authority = Some(up.authority.clone());
        uri_parts.path_and_query = Some(pq.clone());
        let Ok(uri) = Uri::from_parts(uri_parts) else {
            return Ok(status(StatusCode::BAD_GATEWAY, "Bad Gateway\n"));
        };
        let body = match &mut payload {
            Payload::Replay(bytes) => Either::Right(Full::new(bytes.clone())),
            Payload::Stream(stream) => Either::Left(stream.take().expect("body stream hanya dipakai sekali")),
        };
        let mut builder = Request::builder().method(parts.method.clone()).uri(uri).version(Version::HTTP_11);
        if let Some(h) = builder.headers_mut() {
            *h = parts.headers.clone();
            if !route.preserve_host {
                h.insert(HOST, up.host.clone());
            }
        }
        let Ok(request) = builder.body(body) else {
            return Ok(status(StatusCode::BAD_GATEWAY, "Bad Gateway\n"));
        };

        match timeout(shared.timeout, app.client.request(request)).await {
            Ok(Ok(resp)) => {
                up.on_response();
                let took = started.elapsed();
                up.stats.observe(resp.status().as_u16(), took);
                if shared.access_log {
                    println!("{} {} -> {} {} {}ms", parts.method, pq.path(), resp.status().as_u16(), up.authority, took.as_millis());
                }
                let (mut rparts, rbody) = resp.into_parts();
                strip_hop_by_hop(&mut rparts.headers);
                return Ok(Response::from_parts(rparts, Either::Left(rbody)));
            }
            Ok(Err(e)) if e.is_connect() => {
                up.stats.err_connect.fetch_add(1, Relaxed);
                up.eject(shared.health.eject_secs);
                println!("error: upstream {} gagal connect, dikeluarkan {}s: {e}", up.authority, shared.health.eject_secs);
                if attempt == 0 && replayable {
                    if let Some(next) = route.pick_retry(index) {
                        route.upstream(next).stats.retried_in.fetch_add(1, Relaxed);
                        index = next;
                        continue;
                    }
                }
                return Ok(finish(&shared, &parts.method, &pq, StatusCode::BAD_GATEWAY, "Bad Gateway\n", up, started));
            }
            Ok(Err(e)) => {
                up.stats.err_other.fetch_add(1, Relaxed);
                println!("error: upstream {} gagal: {e}", up.authority);
                return Ok(finish(&shared, &parts.method, &pq, StatusCode::BAD_GATEWAY, "Bad Gateway\n", up, started));
            }
            Err(_) => {
                // Tidak diulang: request mungkin sudah diproses upstream.
                up.stats.err_timeout.fetch_add(1, Relaxed);
                up.on_timeout(shared.health.fail_threshold, shared.health.eject_secs);
                println!("error: upstream {} timeout", up.authority);
                return Ok(finish(&shared, &parts.method, &pq, StatusCode::GATEWAY_TIMEOUT, "Gateway Timeout\n", up, started));
            }
        }
    }
    unreachable!("loop selalu return paling lambat pada percobaan kedua")
}

fn finish(
    shared: &Shared,
    method: &hyper::Method,
    pq: &PathAndQuery,
    code: StatusCode,
    msg: &'static str,
    up: &router::Upstream,
    started: Instant,
) -> Response<Body> {
    if shared.access_log {
        println!("{} {} -> {} {} {}ms", method, pq.path(), code.as_u16(), up.authority, started.elapsed().as_millis());
    }
    status(code, msg)
}

/// Probe aktif satu upstream sampai generasi konfigurasinya pensiun.
async fn probe_loop(app: Arc<App>, shared: Arc<Shared>, route_index: usize, upstream_index: usize) {
    let route = &shared.router.routes()[route_index];
    let up = route.upstream(upstream_index);
    let Some(path) = route.health_path.as_deref() else { return };
    let Ok(uri) = format!("http://{}{}", up.authority, path).parse::<Uri>() else { return };
    let every = Duration::from_secs(shared.health.interval_secs);
    let probe_timeout = Duration::from_secs(shared.health.probe_timeout_secs);
    tokio::time::sleep(Duration::from_millis(97 * upstream_index as u64)).await;
    while !shared.retired.load(Relaxed) {
        let request = Request::builder()
            .uri(uri.clone())
            .header(HOST, up.host.clone())
            .body(Either::Right(Full::new(Bytes::new())))
            .expect("request probe valid");
        let ok = matches!(timeout(probe_timeout, app.client.request(request)).await, Ok(Ok(r)) if r.status().is_success());
        match up.on_probe(ok, shared.health.fail_threshold) {
            Some(true) => println!("health: upstream {} sehat kembali", up.authority),
            Some(false) => println!("health: upstream {} DIKELUARKAN (probe {path} gagal)", up.authority),
            None => {}
        }
        tokio::time::sleep(every).await;
    }
}

fn spawn_probes(app: &Arc<App>, shared: &Arc<Shared>) {
    for (ri, route) in shared.router.routes().iter().enumerate() {
        if route.health_path.is_none() {
            continue;
        }
        for ui in 0..route.upstreams().len() {
            tokio::spawn(probe_loop(app.clone(), shared.clone(), ri, ui));
        }
    }
}

fn new_shared(cfg: &config::Config, router: Router) -> Arc<Shared> {
    Arc::new(Shared {
        router,
        timeout: Duration::from_secs(cfg.timeout_secs),
        access_log: cfg.access_log,
        health: cfg.health.clone(),
        retired: AtomicBool::new(false),
    })
}

/// SIGHUP: baca ulang config; bila salah, config lama tetap berlaku. `listen`/`admin_listen` tidak berubah saat reload.
async fn reload_on_sighup(
    app: Arc<App>,
    listen: std::net::SocketAddr,
    admin: Option<std::net::SocketAddr>,
    tls_listen: Option<std::net::SocketAddr>,
) {
    use tokio::signal::unix::{signal, SignalKind};
    let mut hup = signal(SignalKind::hangup()).expect("pasang handler SIGHUP");
    while hup.recv().await.is_some() {
        match load() {
            Ok((cfg, router, acceptor)) => {
                {
                    let mut slot = app.tls.write().unwrap();
                    match (slot.is_some(), acceptor) {
                        (true, Some(a)) => *slot = Some(a),
                        (false, None) => {}
                        _ => println!("peringatan: mengaktifkan/menonaktifkan tls butuh restart, diabaikan"),
                    }
                }
                if cfg.listen != listen || cfg.admin_listen != admin || cfg.tls.as_ref().map(|t| t.listen) != tls_listen {
                    println!("peringatan: perubahan listen/admin_listen/tls.listen butuh restart, diabaikan");
                }
                let fresh = new_shared(&cfg, router);
                let old = std::mem::replace(&mut *app.current.write().unwrap(), fresh.clone());
                old.retired.store(true, Relaxed);
                spawn_probes(&app, &fresh);
                app.global.reloads.fetch_add(1, Relaxed);
                println!("config dimuat ulang ({} routes)", cfg.routes.len());
            }
            Err(e) => {
                app.global.reload_failures.fetch_add(1, Relaxed);
                println!("error: reload gagal, config lama tetap dipakai: {e}");
            }
        }
    }
}

async fn admin(app: Arc<App>, req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    let shared = app.shared();
    let (code, ctype, text) = match req.uri().path() {
        "/healthz" => (StatusCode::OK, "text/plain", "ok\n".to_owned()),
        "/metrics" => {
            let views: Vec<_> = shared
                .router
                .routes()
                .iter()
                .flat_map(|r| {
                    r.upstreams().iter().map(|u| metrics::UpstreamView {
                        route: r.label(),
                        upstream: u.authority.to_string(),
                        up: u.available(),
                        stats: &u.stats,
                    })
                })
                .collect();
            (StatusCode::OK, "text/plain; version=0.0.4", metrics::render(&views, &app.global))
        }
        "/upstreams" => {
            let mut out = String::new();
            for r in shared.router.routes() {
                for u in r.upstreams() {
                    out.push_str(&format!(
                        "{} {} {}\n",
                        r.label(),
                        u.authority,
                        if u.available() { "up" } else { "down" }
                    ));
                }
            }
            (StatusCode::OK, "text/plain", out)
        }
        _ => (StatusCode::NOT_FOUND, "text/plain", "Not Found\n".to_owned()),
    };
    let mut r = Response::new(Full::new(Bytes::from(text)));
    *r.status_mut() = code;
    r.headers_mut().insert(hyper::header::CONTENT_TYPE, HeaderValue::from_static(ctype));
    Ok(r)
}

/// Accept pada listener opsional; tanpa listener, future ini tidak pernah selesai.
async fn accept_opt(l: &Option<TcpListener>) -> std::io::Result<(tokio::net::TcpStream, std::net::SocketAddr)> {
    match l {
        Some(l) => l.accept().await,
        None => std::future::pending().await,
    }
}

async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("pasang handler SIGTERM");
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

fn load() -> Result<(config::Config, Router, Option<tokio_rustls::TlsAcceptor>), String> {
    let path = std::env::var("GATEWAY_CONFIG").unwrap_or_else(|_| "/etc/gateway/config.yaml".into());
    let yaml = std::fs::read_to_string(&path).map_err(|e| format!("tidak bisa membaca {path}: {e}"))?;
    let cfg = config::parse(&yaml).map_err(|e| format!("config {path}: {e}"))?;
    let router = Router::build(&cfg).map_err(|e| format!("config {path}: {e}"))?;
    let acceptor = cfg.tls.as_ref().map(tls::load).transpose().map_err(|e| format!("config {path}: {e}"))?;
    Ok((cfg, router, acceptor))
}

#[tokio::main]
async fn main() {
    let (cfg, router, acceptor) = match load() {
        Ok(v) => v,
        Err(e) => {
            println!("error: {e}");
            std::process::exit(1);
        }
    };
    let listener = match TcpListener::bind(cfg.listen).await {
        Ok(l) => l,
        Err(e) => {
            println!("error: tidak bisa listen di {}: {e}", cfg.listen);
            std::process::exit(1);
        }
    };
    let tls_listener = match &cfg.tls {
        Some(t) => match TcpListener::bind(t.listen).await {
            Ok(l) => Some(l),
            Err(e) => {
                println!("error: tidak bisa listen tls di {}: {e}", t.listen);
                std::process::exit(1);
            }
        },
        None => None,
    };
    let admin_listener = match cfg.admin_listen {
        Some(addr) => match TcpListener::bind(addr).await {
            Ok(l) => Some(l),
            Err(e) => {
                println!("error: tidak bisa listen admin di {addr}: {e}");
                std::process::exit(1);
            }
        },
        None => None,
    };

    let mut connector = HttpConnector::new();
    connector.set_nodelay(true);
    // Gagal connect harus cepat terdeteksi supaya bisa diulang ke upstream lain.
    connector.set_connect_timeout(Some(Duration::from_secs(cfg.timeout_secs.min(3))));
    let client = Client::builder(TokioExecutor::new()).build(connector);
    let shared = new_shared(&cfg, router);
    let app = Arc::new(App {
        current: RwLock::new(shared.clone()),
        tls: RwLock::new(acceptor),
        client,
        global: metrics::Global { no_route: AtomicU64::new(0), reloads: AtomicU64::new(0), reload_failures: AtomicU64::new(0) },
    });
    spawn_probes(&app, &shared);
    tokio::spawn(reload_on_sighup(app.clone(), cfg.listen, cfg.admin_listen, cfg.tls.as_ref().map(|t| t.listen)));

    if let Some(admin_listener) = admin_listener {
        let app = app.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = admin_listener.accept().await else { continue };
                let app = app.clone();
                let svc = service_fn(move |req| admin(app.clone(), req));
                tokio::spawn(async move {
                    let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream), svc).await;
                });
            }
        });
    }

    println!(
        "gateway listening on {}{} ({} routes{})",
        cfg.listen,
        cfg.tls.as_ref().map(|t| format!(", tls {}", t.listen)).unwrap_or_default(),
        cfg.routes.len(),
        cfg.admin_listen.map(|a| format!(", admin {a}")).unwrap_or_default()
    );

    let http = hyper::server::conn::http1::Builder::new();
    let graceful = GracefulShutdown::new();
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        let (r, is_tls) = tokio::select! {
            r = listener.accept() => (r, false),
            r = accept_opt(&tls_listener) => (r, true),
            _ = &mut shutdown => break,
        };
        let (stream, peer) = match r {
            Ok(v) => v,
            Err(e) => {
                println!("error: accept: {e}");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        let acceptor = if is_tls { app.tls.read().unwrap().clone() } else { None };
        let app = app.clone();
        let ip = peer.ip();
        let svc = service_fn(move |req| proxy(app.clone(), ip, is_tls, req));
        match acceptor {
            None => {
                let conn = graceful.watch(http.serve_connection(TokioIo::new(stream), svc));
                tokio::spawn(async move {
                    let _ = conn.await;
                });
            }
            Some(acceptor) => {
                let watcher = graceful.watcher();
                let http = http.clone();
                tokio::spawn(async move {
                    // Handshake dibatasi waktu agar klien lambat/macet tidak menahan koneksi selamanya.
                    let Ok(Ok(tls_stream)) = timeout(Duration::from_secs(10), acceptor.accept(stream)).await else {
                        return;
                    };
                    let _ = watcher.watch(http.serve_connection(TokioIo::new(tls_stream), svc)).await;
                });
            }
        }
    }

    println!("shutting down...");
    drop((listener, tls_listener));
    app.shared().retired.store(true, Relaxed);
    if timeout(Duration::from_secs(cfg.timeout_secs), graceful.shutdown()).await.is_err() {
        println!("shutdown timeout, closing remaining connections");
    }
}

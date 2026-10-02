mod config;
mod router;

use http_body_util::{Either, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{HeaderMap, HeaderName, HeaderValue, CONNECTION, HOST};
use hyper::http::uri::{PathAndQuery, Scheme};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode, Uri, Version};
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::graceful::GracefulShutdown;
use router::Router;
use std::convert::Infallible;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::time::timeout;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

type Body = Either<Incoming, Full<Bytes>>;

struct State {
    router: Router,
    client: Client<HttpConnector, Incoming>,
    timeout: Duration,
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

async fn proxy(state: Arc<State>, peer: IpAddr, req: Request<Incoming>) -> Result<Response<Body>, Infallible> {
    let Some((route, fwd_path)) = state.router.find(req.uri().path()) else {
        return Ok(status(StatusCode::NOT_FOUND, "Not Found\n"));
    };
    let upstream = route.pick();

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
    let mut up = hyper::http::uri::Parts::default();
    up.scheme = Some(Scheme::HTTP);
    up.authority = Some(upstream.authority.clone());
    up.path_and_query = Some(pq);
    let Ok(uri) = Uri::from_parts(up) else {
        return Ok(status(StatusCode::BAD_GATEWAY, "Bad Gateway\n"));
    };
    parts.uri = uri;
    parts.version = Version::HTTP_11;
    strip_hop_by_hop(&mut parts.headers);
    parts.headers.insert(HOST, upstream.host.clone());
    add_forwarded_for(&mut parts.headers, peer);

    let req = Request::from_parts(parts, body);
    match timeout(state.timeout, state.client.request(req)).await {
        Ok(Ok(resp)) => {
            let (mut parts, body) = resp.into_parts();
            strip_hop_by_hop(&mut parts.headers);
            Ok(Response::from_parts(parts, Either::Left(body)))
        }
        Ok(Err(e)) => {
            println!("error: upstream {} gagal: {e}", upstream.authority);
            Ok(status(StatusCode::BAD_GATEWAY, "Bad Gateway\n"))
        }
        Err(_) => {
            println!("error: upstream {} timeout", upstream.authority);
            Ok(status(StatusCode::GATEWAY_TIMEOUT, "Gateway Timeout\n"))
        }
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

fn load() -> Result<(config::Config, Router), String> {
    let path = std::env::var("GATEWAY_CONFIG").unwrap_or_else(|_| "/etc/gateway/config.yaml".into());
    let yaml = std::fs::read_to_string(&path).map_err(|e| format!("tidak bisa membaca {path}: {e}"))?;
    let cfg = config::parse(&yaml).map_err(|e| format!("config {path}: {e}"))?;
    let router = Router::build(&cfg).map_err(|e| format!("config {path}: {e}"))?;
    Ok((cfg, router))
}

#[tokio::main]
async fn main() {
    let (cfg, router) = match load() {
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

    let mut connector = HttpConnector::new();
    connector.set_nodelay(true);
    let client = Client::builder(TokioExecutor::new()).build(connector);
    let state = Arc::new(State { router, client, timeout: Duration::from_secs(cfg.timeout_secs) });

    println!("gateway listening on {} ({} routes)", cfg.listen, cfg.routes.len());

    let http = hyper::server::conn::http1::Builder::new();
    let graceful = GracefulShutdown::new();
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        let (stream, peer) = tokio::select! {
            r = listener.accept() => match r {
                Ok(v) => v,
                Err(e) => {
                    println!("error: accept: {e}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            },
            _ = &mut shutdown => break,
        };
        let _ = stream.set_nodelay(true);
        let state = state.clone();
        let ip = peer.ip();
        let svc = service_fn(move |req| proxy(state.clone(), ip, req));
        let conn = http.serve_connection(TokioIo::new(stream), svc);
        let conn = graceful.watch(conn);
        tokio::spawn(async move {
            let _ = conn.await;
        });
    }

    println!("shutting down...");
    drop(listener);
    if timeout(Duration::from_secs(cfg.timeout_secs), graceful.shutdown()).await.is_err() {
        println!("shutdown timeout, closing remaining connections");
    }
}

use crate::config::{normalize_path, parse_host, split_host_port, Config, HostRule};
use hyper::header::HeaderValue;
use hyper::http::uri::{Authority, Scheme};
use hyper::Uri;
use crate::metrics::Stats;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

const SLOTS: usize = 100;

/// Milidetik sejak proses mulai; dipakai untuk batas waktu ejeksi tanpa lock.
fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

pub struct Upstream {
    pub authority: Authority,
    pub host: HeaderValue,
    pub stats: Stats,
    /// Hasil probe aktif; tanpa `health_path` selalu true.
    healthy: AtomicBool,
    probe_failures: AtomicU32,
    timeouts_in_row: AtomicU32,
    ejected_until_ms: AtomicU64,
}

impl Upstream {
    fn new(authority: Authority, host: HeaderValue) -> Upstream {
        Upstream {
            authority,
            host,
            stats: Stats::default(),
            healthy: AtomicBool::new(true),
            probe_failures: AtomicU32::new(0),
            timeouts_in_row: AtomicU32::new(0),
            ejected_until_ms: AtomicU64::new(0),
        }
    }

    /// Ikut rotasi: lolos probe aktif dan tidak sedang dikeluarkan.
    pub fn available(&self) -> bool {
        self.healthy.load(Ordering::Relaxed) && now_ms() >= self.ejected_until_ms.load(Ordering::Relaxed)
    }

    /// Keluarkan dari rotasi selama `secs`; setelah itu request berikutnya mencoba lagi (half-open).
    pub fn eject(&self, secs: u64) {
        self.ejected_until_ms.store(now_ms() + secs * 1000, Ordering::Relaxed);
        self.stats.ejections.fetch_add(1, Ordering::Relaxed);
    }

    pub fn on_response(&self) {
        self.timeouts_in_row.store(0, Ordering::Relaxed);
        self.ejected_until_ms.store(0, Ordering::Relaxed);
    }

    /// Timeout menunggu header: dikeluarkan setelah `threshold` kali berturut-turut.
    pub fn on_timeout(&self, threshold: u32, eject_secs: u64) {
        if self.timeouts_in_row.fetch_add(1, Ordering::Relaxed) + 1 >= threshold {
            self.timeouts_in_row.store(0, Ordering::Relaxed);
            self.eject(eject_secs);
        }
    }

    /// Hasil satu probe aktif. Mengembalikan `Some(status_baru)` bila status sehat/tidak berubah.
    pub fn on_probe(&self, ok: bool, threshold: u32) -> Option<bool> {
        if ok {
            self.probe_failures.store(0, Ordering::Relaxed);
            self.ejected_until_ms.store(0, Ordering::Relaxed);
            (!self.healthy.swap(true, Ordering::Relaxed)).then_some(true)
        } else if self.probe_failures.fetch_add(1, Ordering::Relaxed) + 1 >= threshold {
            self.healthy.swap(false, Ordering::Relaxed).then_some(false)
        } else {
            None
        }
    }
}

pub struct Route {
    host: Option<HostRule>,
    /// Urutan kekhususan host (kecil = lebih spesifik): host+port, host, wildcard+port, wildcard, tanpa host.
    rank: u8,
    /// Label untuk metrics/log: path saja bila tanpa host, selain itu `host[:port]` + path.
    label: String,
    pub preserve_host: bool,
    prefix: String,
    pub strip_prefix: bool,
    pub health_path: Option<String>,
    upstreams: Box<[Upstream]>,
    /// Tabel 100 slot berisi indeks upstream, sudah diinterleave.
    table: [u8; SLOTS],
    next: AtomicUsize,
}

pub struct Router {
    /// Terurut dari host paling spesifik lalu prefix terpanjang, sehingga kecocokan pertama = yang terbaik.
    routes: Box<[Route]>,
}

impl Route {
    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn upstreams(&self) -> &[Upstream] {
        &self.upstreams
    }

    pub fn upstream(&self, index: usize) -> &Upstream {
        &self.upstreams[index]
    }

    /// Pilih upstream sesuai persentase weight, tanpa lock. Upstream yang dikeluarkan dilewati ke slot
    /// berikutnya (porsinya terbagi ke yang sehat); bila semua dikeluarkan, tetap pakai pilihan slot (fail-open).
    #[inline]
    pub fn pick(&self) -> usize {
        if self.upstreams.len() == 1 {
            return 0;
        }
        let slot = self.next.fetch_add(1, Ordering::Relaxed) % SLOTS;
        let first = self.table[slot] as usize;
        if self.upstreams[first].available() {
            return first;
        }
        (1..SLOTS)
            .map(|d| self.table[(slot + d) % SLOTS] as usize)
            .find(|&i| self.upstreams[i].available())
            .unwrap_or(first)
    }

    /// Upstream lain yang tersedia untuk mengulang request setelah `failed` gagal connect.
    pub fn pick_retry(&self, failed: usize) -> Option<usize> {
        if self.upstreams.len() == 1 {
            return None;
        }
        let slot = self.next.fetch_add(1, Ordering::Relaxed) % SLOTS;
        (0..SLOTS)
            .map(|d| self.table[(slot + d) % SLOTS] as usize)
            .find(|&i| i != failed && self.upstreams[i].available())
    }
}

impl Router {
    pub fn build(cfg: &Config) -> Result<Router, String> {
        let mut routes = Vec::with_capacity(cfg.routes.len());
        for (i, rc) in cfg.routes.iter().enumerate() {
            let mut upstreams = Vec::new();
            let mut weights = Vec::new();
            for (j, u) in rc.upstreams.iter().enumerate() {
                let up = parse_upstream(&u.url)
                    .map_err(|e| format!("routes[{i}].upstreams[{j}]: {e}"))?;
                upstreams.push(up);
                weights.push(u.weight);
            }
            let host = rc.host.as_deref().map(parse_host).transpose().map_err(|e| format!("routes[{i}]: {e}"))?;
            let prefix = normalize_path(&rc.path).to_owned();
            let path_label = if prefix.is_empty() { "/" } else { prefix.as_str() };
            let (rank, label) = match &host {
                None => (4, path_label.to_owned()),
                Some(h) => {
                    let port = h.port.map(|p| format!(":{p}")).unwrap_or_default();
                    let label = format!("{}{}{port}{path_label}", if h.wildcard { "*." } else { "" }, h.name);
                    (u8::from(h.wildcard) * 2 + u8::from(h.port.is_none()), label)
                }
            };
            routes.push(Route {
                host,
                rank,
                label,
                preserve_host: rc.preserve_host,
                prefix,
                strip_prefix: rc.strip_prefix,
                health_path: rc.health_path.clone(),
                upstreams: upstreams.into_boxed_slice(),
                table: build_table(&weights),
                next: AtomicUsize::new(0),
            });
        }
        routes.sort_by(|a, b| a.rank.cmp(&b.rank).then(b.prefix.len().cmp(&a.prefix.len())));
        Ok(Router { routes: routes.into_boxed_slice() })
    }

    pub fn routes(&self) -> &[Route] {
        &self.routes
    }

    /// Cari rute: host dulu (host+port > host > wildcard+port > wildcard > tanpa host), lalu longest prefix
    /// pada batas segmen. `host` = header Host klien; `tls` menentukan port default (443/80) bila tak ditulis.
    /// Mengembalikan rute dan path yang harus diteruskan ke upstream.
    pub fn find<'a, 'p>(&'a self, host: Option<&str>, tls: bool, path: &'p str) -> Option<(&'a Route, &'p str)> {
        let req = host.map(|h| {
            let (name, port) = split_host_port(h);
            (name, port.unwrap_or(if tls { 443 } else { 80 }))
        });
        for r in self.routes.iter() {
            if let Some(rule) = &r.host {
                match req {
                    Some((name, port)) if host_matches(rule, name, port) => {}
                    _ => continue,
                }
            }
            if let Some(rest) = path.strip_prefix(r.prefix.as_str()) {
                // Batas segmen: sisa kosong atau diawali '/'. (prefix "" selalu lolos untuk path '/...')
                if rest.is_empty() || rest.starts_with('/') {
                    let fwd = if !r.strip_prefix {
                        path
                    } else if rest.is_empty() {
                        "/"
                    } else {
                        rest
                    };
                    return Some((r, fwd));
                }
            }
        }
        None
    }

    #[cfg(test)]
    fn find_path<'a, 'p>(&'a self, path: &'p str) -> Option<(&'a Route, &'p str)> {
        self.find(None, false, path)
    }
}

/// Wildcard `*.x.com` cocok untuk tepat satu label di depan `x.com` (bukan `x.com` sendiri, bukan `a.b.x.com`).
fn host_matches(rule: &HostRule, name: &str, port: u16) -> bool {
    if rule.port.is_some_and(|p| p != port) {
        return false;
    }
    if !rule.wildcard {
        return name.eq_ignore_ascii_case(&rule.name);
    }
    let (n, suffix) = (name.len(), rule.name.len());
    n > suffix + 1
        && name.is_char_boundary(n - suffix)
        && name.as_bytes()[n - suffix - 1] == b'.'
        && name[n - suffix..].eq_ignore_ascii_case(&rule.name)
        && !name[..n - suffix - 1].contains('.')
}

fn parse_upstream(url: &str) -> Result<Upstream, String> {
    let uri: Uri = url.parse().map_err(|e| format!("url '{url}' tidak valid: {e}"))?;
    if uri.scheme() != Some(&Scheme::HTTP) {
        return Err(format!("url '{url}': hanya skema http:// yang didukung"));
    }
    let authority = uri.authority().cloned().ok_or_else(|| format!("url '{url}': host tidak ada"))?;
    if (uri.path() != "/" && !uri.path().is_empty()) || uri.query().is_some() {
        return Err(format!("url '{url}': tidak boleh berisi path atau query"));
    }
    let host = HeaderValue::from_str(authority.as_str()).map_err(|e| e.to_string())?;
    Ok(Upstream::new(authority, host))
}

/// Smooth weighted round-robin untuk 100 slot: hasilnya tepat sesuai weight dan
/// slot tiap upstream tersebar merata (bukan berurutan).
fn build_table(weights: &[u32]) -> [u8; SLOTS] {
    let mut table = [0u8; SLOTS];
    let mut cur = vec![0i32; weights.len()];
    for slot in table.iter_mut() {
        let mut best = 0;
        for (i, w) in weights.iter().enumerate() {
            cur[i] += *w as i32;
            if cur[i] > cur[best] {
                best = i;
            }
        }
        cur[best] -= SLOTS as i32;
        *slot = best as u8;
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;

    fn router(yaml: &str) -> Router {
        Router::build(&config::parse(yaml).unwrap()).unwrap()
    }

    const YAML: &str = r#"
routes:
  - path: /api
    upstreams:
      - {url: "http://a:1", weight: 80}
      - {url: "http://b:2", weight: 20}
  - path: /api/something
    strip_prefix: true
    upstreams:
      - {url: "http://c:3", weight: 100}
"#;

    fn host_of(r: &Route) -> String {
        r.upstream(r.pick()).authority.to_string()
    }

    #[test]
    fn longest_prefix_wins() {
        let r = router(YAML);
        assert_eq!(host_of(r.find_path("/api/something/123").unwrap().0), "c:3");
        assert_eq!(host_of(r.find_path("/api/something").unwrap().0), "c:3");
        let (route, _) = r.find_path("/api/other").unwrap();
        assert!(matches!(host_of(route).as_str(), "a:1" | "b:2"));
        assert!(r.find_path("/api").is_some());
    }

    #[test]
    fn matches_on_segment_boundary() {
        let r = router(YAML);
        assert!(r.find_path("/apix").is_none());
        assert!(r.find_path("/api2/x").is_none());
        // /api/somethingelse jatuh ke /api, bukan /api/something
        let (route, _) = r.find_path("/api/somethingelse").unwrap();
        assert!(!route.strip_prefix);
        assert!(r.find_path("/other").is_none());
        assert!(r.find_path("/").is_none());
    }

    #[test]
    fn root_route_matches_everything() {
        let r = router("routes:\n  - path: /\n    upstreams:\n      - {url: 'http://a:1', weight: 100}\n");
        assert!(r.find_path("/").is_some());
        assert!(r.find_path("/anything/here").is_some());
    }

    #[test]
    fn strip_prefix_rewrites_path() {
        let r = router(YAML);
        assert_eq!(r.find_path("/api/something/123").unwrap().1, "/123");
        assert_eq!(r.find_path("/api/something").unwrap().1, "/");
        assert_eq!(r.find_path("/api/something/").unwrap().1, "/");
        // tanpa strip_prefix path tidak berubah
        assert_eq!(r.find_path("/api/x").unwrap().1, "/api/x");
    }

    #[test]
    fn weight_distribution_matches_percentages() {
        let r = router(YAML);
        let (route, _) = r.find_path("/api").unwrap();
        let mut a = 0;
        for _ in 0..10_000 {
            if route.upstream(route.pick()).authority.as_str() == "a:1" {
                a += 1;
            }
        }
        // Tabel 100 slot -> 10.000 pemilihan harus tepat 80/20.
        assert_eq!(a, 8_000);
    }

    #[test]
    fn table_is_interleaved_and_exact() {
        let t = build_table(&[80, 20]);
        assert_eq!(t.iter().filter(|&&x| x == 1).count(), 20);
        // tidak ada run panjang upstream 1 (tidak berurutan)
        let longest_run = t.split(|&x| x == 1).map(|s| s.len()).max().unwrap();
        assert!(longest_run <= 4, "run terlalu panjang: {longest_run}");
        let t = build_table(&[50, 30, 20]);
        for (i, n) in [50, 30, 20].iter().enumerate() {
            assert_eq!(t.iter().filter(|&&x| x as usize == i).count(), *n);
        }
    }

    #[test]
    fn ejected_upstream_is_skipped_and_its_share_goes_to_the_healthy_one() {
        let r = router(YAML);
        let (route, _) = r.find_path("/api").unwrap();
        route.upstream(0).eject(60);
        assert!(!route.upstream(0).available());
        for _ in 0..500 {
            assert_eq!(route.pick(), 1);
        }
    }

    #[test]
    fn all_ejected_fails_open() {
        let r = router(YAML);
        let (route, _) = r.find_path("/api").unwrap();
        route.upstream(0).eject(60);
        route.upstream(1).eject(60);
        let picks: std::collections::HashSet<_> = (0..200).map(|_| route.pick()).collect();
        assert_eq!(picks.len(), 2, "fail-open harus tetap membagi sesuai tabel");
    }

    #[test]
    fn retry_picks_another_available_upstream_only() {
        let r = router(YAML);
        let (route, _) = r.find_path("/api").unwrap();
        assert_eq!(route.pick_retry(0), Some(1));
        assert_eq!(route.pick_retry(1), Some(0));
        route.upstream(1).eject(60);
        assert_eq!(route.pick_retry(0), None);
        let (single, _) = r.find_path("/api/something").unwrap();
        assert_eq!(single.pick_retry(0), None);
    }

    #[test]
    fn probe_threshold_and_recovery() {
        let r = router(YAML);
        let (route, _) = r.find_path("/api").unwrap();
        let u = route.upstream(0);
        assert_eq!(u.on_probe(false, 2), None);
        assert!(u.available());
        assert_eq!(u.on_probe(false, 2), Some(false));
        assert!(!u.available());
        assert_eq!(u.on_probe(false, 2), None, "tidak ada transisi ulang");
        assert_eq!(u.on_probe(true, 2), Some(true));
        assert!(u.available());
    }

    #[test]
    fn timeouts_eject_only_after_threshold_in_a_row() {
        let r = router(YAML);
        let (route, _) = r.find_path("/api").unwrap();
        let u = route.upstream(1);
        u.on_timeout(2, 30);
        assert!(u.available());
        u.on_response();
        u.on_timeout(2, 30);
        assert!(u.available(), "respons sukses mereset hitungan");
        u.on_timeout(2, 30);
        assert!(!u.available());
    }

    #[test]
    fn rejects_bad_upstream_url() {
        for bad in ["https://a:1", "a:1", "http://a:1/path"] {
            let y = format!("routes:\n  - path: /\n    upstreams:\n      - {{url: '{bad}', weight: 100}}\n");
            assert!(Router::build(&config::parse(&y).unwrap()).is_err(), "{bad}");
        }
    }

    #[test]
    fn host_routing_priority_and_matching() {
        let r = router(
            "routes:
  - {path: /, upstreams: [{url: 'http://fallback:1', weight: 100}]}
  - {host: '*.cashlez.com', path: /, upstreams: [{url: 'http://wild:1', weight: 100}]}
  - {host: api.cashlez.com, path: /, upstreams: [{url: 'http://exact:1', weight: 100}]}
  - {host: api.cashlez.com, path: /v1, upstreams: [{url: 'http://exactv1:1', weight: 100}]}
  - {host: 'api.cashlez.com:8443', path: /, upstreams: [{url: 'http://exactport:1', weight: 100}]}
  - {host: 192.168.90.46, path: /, upstreams: [{url: 'http://ip:1', weight: 100}]}
  - {host: '192.168.90.46:8080', path: /, upstreams: [{url: 'http://ipport:1', weight: 100}]}
",
        );
        let up = |h: Option<&str>, tls: bool, p: &str| host_of(r.find(h, tls, p).unwrap().0);
        assert_eq!(up(Some("api.cashlez.com"), false, "/x"), "exact:1");
        assert_eq!(up(Some("API.Cashlez.COM"), false, "/v1/x"), "exactv1:1"); // case-insensitive + longest prefix
        assert_eq!(up(Some("api.cashlez.com:443"), true, "/x"), "exact:1"); // port bukan 8443
        assert_eq!(up(Some("api.cashlez.com:8443"), true, "/x"), "exactport:1"); // host+port menang
        assert_eq!(up(Some("api.cashlez.com:8443"), true, "/v1/x"), "exactport:1"); // lebih spesifik dari path
        assert_eq!(up(Some("app.cashlez.com"), false, "/x"), "wild:1");
        assert_eq!(up(Some("a.b.cashlez.com"), false, "/x"), "fallback:1"); // wildcard satu level
        assert_eq!(up(Some("cashlez.com"), false, "/x"), "fallback:1"); // apex bukan wildcard
        assert_eq!(up(Some("evilcashlez.com"), false, "/x"), "fallback:1");
        assert_eq!(up(Some("192.168.90.46"), false, "/x"), "ip:1");
        assert_eq!(up(Some("192.168.90.46:9999"), false, "/x"), "ip:1"); // tanpa port di config = port apa pun
        assert_eq!(up(Some("192.168.90.46:8080"), false, "/x"), "ipport:1");
        assert_eq!(up(Some("unknown.example"), false, "/x"), "fallback:1");
        assert_eq!(up(None, false, "/x"), "fallback:1"); // tanpa Host hanya rute tanpa host
    }

    #[test]
    fn host_default_port_depends_on_listener() {
        let r = router(
            "routes:
  - {host: 'x.com:443', path: /, upstreams: [{url: 'http://https:1', weight: 100}]}
  - {host: 'x.com:80', path: /, upstreams: [{url: 'http://http:1', weight: 100}]}
",
        );
        assert_eq!(host_of(r.find(Some("x.com"), true, "/").unwrap().0), "https:1");
        assert_eq!(host_of(r.find(Some("x.com"), false, "/").unwrap().0), "http:1");
        assert!(r.find(Some("x.com:8080"), false, "/").is_none());
    }

    #[test]
    fn no_host_routes_only_when_no_fallback() {
        let r = router("routes:\n  - {host: a.com, path: /, upstreams: [{url: 'http://a:1', weight: 100}]}\n");
        assert!(r.find(Some("b.com"), false, "/").is_none());
        assert!(r.find(None, false, "/").is_none());
    }

    #[test]
    fn ipv6_host() {
        let r = router("routes:\n  - {host: '[::1]:8080', path: /, upstreams: [{url: 'http://a:1', weight: 100}]}\n");
        assert!(r.find(Some("[::1]:8080"), false, "/").is_some());
        assert!(r.find(Some("[::1]:9"), false, "/").is_none());
    }

    #[test]
    fn labels_and_preserve_host() {
        let r = router(
            "routes:
  - {path: /, upstreams: [{url: 'http://a:1', weight: 100}]}
  - {host: '*.x.com:8443', path: /v1/, preserve_host: true, upstreams: [{url: 'http://b:1', weight: 100}]}
",
        );
        let labels: Vec<_> = r.routes().iter().map(|x| x.label().to_owned()).collect();
        assert_eq!(labels, ["*.x.com:8443/v1", "/"]);
        assert!(r.routes()[0].preserve_host && !r.routes()[1].preserve_host);
    }
}

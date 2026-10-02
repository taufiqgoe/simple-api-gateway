use crate::config::{normalize_path, Config};
use hyper::header::HeaderValue;
use hyper::http::uri::{Authority, Scheme};
use hyper::Uri;
use std::sync::atomic::{AtomicUsize, Ordering};

const SLOTS: usize = 100;

pub struct Upstream {
    pub authority: Authority,
    pub host: HeaderValue,
}

pub struct Route {
    prefix: String,
    pub strip_prefix: bool,
    upstreams: Box<[Upstream]>,
    /// Tabel 100 slot berisi indeks upstream, sudah diinterleave.
    table: [u8; SLOTS],
    next: AtomicUsize,
}

pub struct Router {
    /// Terurut dari prefix terpanjang ke terpendek, sehingga kecocokan pertama = longest match.
    routes: Box<[Route]>,
}

impl Route {
    /// Pilih upstream sesuai persentase weight, tanpa lock.
    #[inline]
    pub fn pick(&self) -> &Upstream {
        if self.upstreams.len() == 1 {
            return &self.upstreams[0];
        }
        let slot = self.next.fetch_add(1, Ordering::Relaxed) % SLOTS;
        &self.upstreams[self.table[slot] as usize]
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
            routes.push(Route {
                prefix: normalize_path(&rc.path).to_owned(),
                strip_prefix: rc.strip_prefix,
                upstreams: upstreams.into_boxed_slice(),
                table: build_table(&weights),
                next: AtomicUsize::new(0),
            });
        }
        routes.sort_by(|a, b| b.prefix.len().cmp(&a.prefix.len()));
        Ok(Router { routes: routes.into_boxed_slice() })
    }

    /// Cari rute dengan longest prefix match pada batas segmen.
    /// Mengembalikan rute dan path yang harus diteruskan ke upstream.
    pub fn find<'a, 'p>(&'a self, path: &'p str) -> Option<(&'a Route, &'p str)> {
        for r in self.routes.iter() {
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
    Ok(Upstream { authority, host })
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
        r.pick().authority.to_string()
    }

    #[test]
    fn longest_prefix_wins() {
        let r = router(YAML);
        assert_eq!(host_of(r.find("/api/something/123").unwrap().0), "c:3");
        assert_eq!(host_of(r.find("/api/something").unwrap().0), "c:3");
        let (route, _) = r.find("/api/other").unwrap();
        assert!(matches!(host_of(route).as_str(), "a:1" | "b:2"));
        assert!(r.find("/api").is_some());
    }

    #[test]
    fn matches_on_segment_boundary() {
        let r = router(YAML);
        assert!(r.find("/apix").is_none());
        assert!(r.find("/api2/x").is_none());
        // /api/somethingelse jatuh ke /api, bukan /api/something
        let (route, _) = r.find("/api/somethingelse").unwrap();
        assert!(!route.strip_prefix);
        assert!(r.find("/other").is_none());
        assert!(r.find("/").is_none());
    }

    #[test]
    fn root_route_matches_everything() {
        let r = router("routes:\n  - path: /\n    upstreams:\n      - {url: 'http://a:1', weight: 100}\n");
        assert!(r.find("/").is_some());
        assert!(r.find("/anything/here").is_some());
    }

    #[test]
    fn strip_prefix_rewrites_path() {
        let r = router(YAML);
        assert_eq!(r.find("/api/something/123").unwrap().1, "/123");
        assert_eq!(r.find("/api/something").unwrap().1, "/");
        assert_eq!(r.find("/api/something/").unwrap().1, "/");
        // tanpa strip_prefix path tidak berubah
        assert_eq!(r.find("/api/x").unwrap().1, "/api/x");
    }

    #[test]
    fn weight_distribution_matches_percentages() {
        let r = router(YAML);
        let (route, _) = r.find("/api").unwrap();
        let mut a = 0;
        for _ in 0..10_000 {
            if route.pick().authority.as_str() == "a:1" {
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
    fn rejects_bad_upstream_url() {
        for bad in ["https://a:1", "a:1", "http://a:1/path"] {
            let y = format!("routes:\n  - path: /\n    upstreams:\n      - {{url: '{bad}', weight: 100}}\n");
            assert!(Router::build(&config::parse(&y).unwrap()).is_err(), "{bad}");
        }
    }
}

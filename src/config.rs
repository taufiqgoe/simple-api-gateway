use serde::Deserialize;
use std::net::SocketAddr;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    pub routes: Vec<RouteConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteConfig {
    pub path: String,
    #[serde(default)]
    pub strip_prefix: bool,
    pub upstreams: Vec<UpstreamConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    pub url: String,
    pub weight: u32,
}

fn default_listen() -> SocketAddr {
    ([0, 0, 0, 0], 3000).into()
}

fn default_timeout() -> u64 {
    30
}

/// Parse YAML lalu validasi. Pesan error menyebut lokasi masalahnya.
pub fn parse(yaml: &str) -> Result<Config, String> {
    let cfg: Config = serde_yaml_ng::from_str(yaml).map_err(|e| e.to_string())?;
    validate(&cfg)?;
    Ok(cfg)
}

/// Path rute dinormalkan: tanpa trailing slash, dan "/" menjadi "" (cocok dengan semua).
pub fn normalize_path(path: &str) -> &str {
    path.trim_end_matches('/')
}

fn validate(cfg: &Config) -> Result<(), String> {
    if cfg.timeout_secs == 0 {
        return Err("timeout_secs harus > 0".into());
    }
    if cfg.routes.is_empty() {
        return Err("routes tidak boleh kosong".into());
    }
    for (i, r) in cfg.routes.iter().enumerate() {
        if !r.path.starts_with('/') {
            return Err(format!("routes[{i}]: path '{}' harus diawali '/'", r.path));
        }
        let norm = normalize_path(&r.path);
        if cfg.routes[..i].iter().any(|p| normalize_path(&p.path) == norm) {
            return Err(format!("routes[{i}]: path '{}' duplikat", r.path));
        }
        if r.upstreams.is_empty() {
            return Err(format!("routes[{i}] ({}): upstreams tidak boleh kosong", r.path));
        }
        let mut total = 0u32;
        for (j, u) in r.upstreams.iter().enumerate() {
            if !(1..=100).contains(&u.weight) {
                return Err(format!(
                    "routes[{i}] ({}).upstreams[{j}]: weight {} di luar 1..=100",
                    r.path, u.weight
                ));
            }
            total += u.weight;
        }
        if total != 100 {
            return Err(format!(
                "routes[{i}] ({}): total weight harus 100, ditemukan {total}",
                r.path
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: &str = r#"
listen: "0.0.0.0:3000"
timeout_secs: 10
routes:
  - path: /api/something
    upstreams:
      - url: http://localhost:8081
        weight: 100
  - path: /api
    strip_prefix: true
    upstreams:
      - url: http://localhost:8080
        weight: 80
      - url: http://localhost:8082
        weight: 20
"#;

    #[test]
    fn parses_valid_config() {
        let c = parse(OK).unwrap();
        assert_eq!(c.listen.port(), 3000);
        assert_eq!(c.timeout_secs, 10);
        assert_eq!(c.routes.len(), 2);
        assert!(!c.routes[0].strip_prefix);
        assert!(c.routes[1].strip_prefix);
        assert_eq!(c.routes[1].upstreams[0].weight, 80);
    }

    #[test]
    fn defaults_apply() {
        let c = parse("routes:\n  - path: /\n    upstreams:\n      - {url: 'http://a:1', weight: 100}\n")
            .unwrap();
        assert_eq!(c.timeout_secs, 30);
        assert_eq!(c.listen.port(), 3000);
    }

    #[test]
    fn unknown_field_is_rejected_with_location() {
        let y = OK.replace("timeout_secs: 10", "timeout_sec: 10");
        let e = parse(&y).unwrap_err();
        assert!(e.contains("unknown field `timeout_sec`"), "{e}");
        assert!(e.contains("line"), "{e}");

        let y = OK.replace("weight: 100", "weight: 100\n        wieght: 1");
        assert!(parse(&y).unwrap_err().contains("unknown field `wieght`"));
    }

    #[test]
    fn weight_total_must_be_100() {
        let e = parse(&OK.replace("weight: 20", "weight: 30")).unwrap_err();
        assert!(e.contains("total weight harus 100, ditemukan 110"), "{e}");
        assert!(e.contains("/api"), "{e}");
        let e = parse(&OK.replace("weight: 20", "weight: 10")).unwrap_err();
        assert!(e.contains("ditemukan 90"), "{e}");
    }

    #[test]
    fn rejects_bad_paths_and_zero_weight() {
        assert!(parse(&OK.replace("path: /api/something", "path: api")).is_err());
        assert!(parse(&OK.replace("path: /api/something", "path: /api/")).is_err()); // duplikat /api
        assert!(parse(&OK.replace("weight: 80", "weight: 0").replace("weight: 20", "weight: 100")).is_err());
    }
}

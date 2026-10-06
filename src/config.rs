use serde::Deserialize;
use std::net::SocketAddr;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Listener admin (`/metrics`, `/healthz`, `/upstreams`); tanpa field ini listener tidak dibuka.
    #[serde(default)]
    pub admin_listen: Option<SocketAddr>,
    /// Satu baris stdout per request: `METHOD path -> status upstream ms`.
    #[serde(default)]
    pub access_log: bool,
    #[serde(default)]
    pub health: HealthConfig,
    /// Listener HTTPS tambahan (TLS termination). Tanpa blok ini hanya HTTP pada `listen`.
    #[serde(default)]
    pub tls: Option<TlsConfig>,
    pub routes: Vec<RouteConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    /// Alamat HTTPS. `listen` tetap HTTP biasa; keduanya dilayani bersamaan.
    pub listen: SocketAddr,
    /// PEM berisi sertifikat server diikuti sertifikat intermediate (fullchain).
    pub cert: String,
    /// PEM private key (PKCS#1, PKCS#8, atau SEC1).
    pub key: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthConfig {
    /// Jeda antar probe aktif (hanya rute dengan `health_path`).
    #[serde(default = "two")]
    pub interval_secs: u64,
    #[serde(default = "two")]
    pub probe_timeout_secs: u64,
    /// Probe gagal / timeout berurutan sebelum upstream dikeluarkan dari rotasi.
    #[serde(default = "two")]
    pub fail_threshold: u32,
    /// Lama upstream dikeluarkan setelah gagal connect (atau timeout berulang); lalu dicoba lagi.
    #[serde(default = "ten")]
    pub eject_secs: u64,
    /// Body request sampai batas ini disangga di memori supaya gagal-connect bisa diulang ke upstream lain.
    #[serde(default = "default_retry_body_limit")]
    pub retry_body_limit_bytes: u64,
}

impl Default for HealthConfig {
    fn default() -> Self {
        HealthConfig {
            interval_secs: 2,
            probe_timeout_secs: 2,
            fail_threshold: 2,
            eject_secs: 10,
            retry_body_limit_bytes: default_retry_body_limit(),
        }
    }
}

fn two<T: From<u8>>() -> T {
    T::from(2)
}

fn ten() -> u64 {
    10
}

fn default_retry_body_limit() -> u64 {
    256 * 1024
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteConfig {
    pub path: String,
    #[serde(default)]
    pub strip_prefix: bool,
    /// Path probe aktif (GET, sukses = 2xx) ke tiap upstream rute ini, mis. `/actuator/health`.
    #[serde(default)]
    pub health_path: Option<String>,
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
    if let Some(t) = &cfg.tls {
        if t.cert.is_empty() || t.key.is_empty() {
            return Err("tls: cert dan key tidak boleh kosong".into());
        }
        if t.listen == cfg.listen || Some(t.listen) == cfg.admin_listen {
            return Err(format!("tls.listen {} bentrok dengan listen/admin_listen", t.listen));
        }
    }
    let h = &cfg.health;
    if h.interval_secs == 0 || h.probe_timeout_secs == 0 || h.fail_threshold == 0 || h.eject_secs == 0 {
        return Err("health: interval_secs, probe_timeout_secs, fail_threshold, eject_secs harus > 0".into());
    }
    for (i, r) in cfg.routes.iter().enumerate() {
        if !r.path.starts_with('/') {
            return Err(format!("routes[{i}]: path '{}' harus diawali '/'", r.path));
        }
        let norm = normalize_path(&r.path);
        if cfg.routes[..i].iter().any(|p| normalize_path(&p.path) == norm) {
            return Err(format!("routes[{i}]: path '{}' duplikat", r.path));
        }
        if let Some(hp) = &r.health_path {
            if !hp.starts_with('/') {
                return Err(format!("routes[{i}] ({}): health_path '{hp}' harus diawali '/'", r.path));
            }
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
    fn health_admin_and_log_options_parse_with_defaults() {
        let c = parse(OK).unwrap();
        assert!(c.admin_listen.is_none() && !c.access_log);
        assert_eq!((c.health.interval_secs, c.health.fail_threshold, c.health.eject_secs), (2, 2, 10));
        let y = format!(
            "admin_listen: '127.0.0.1:3001'\naccess_log: true\nhealth: {{interval_secs: 5, fail_threshold: 3}}\n{}",
            OK.replace("  - path: /api\n", "  - path: /api\n    health_path: /health\n")
        );
        let c = parse(&y).unwrap();
        assert_eq!(c.admin_listen.unwrap().port(), 3001);
        assert!(c.access_log);
        assert_eq!((c.health.interval_secs, c.health.fail_threshold), (5, 3));
        assert_eq!(c.routes[1].health_path.as_deref(), Some("/health"));
    }

    #[test]
    fn tls_block_parses_and_is_optional() {
        assert!(parse(OK).unwrap().tls.is_none());
        let c = parse(&format!("tls: {{listen: '0.0.0.0:3443', cert: /c.pem, key: /k.pem}}\n{OK}")).unwrap();
        let t = c.tls.unwrap();
        assert_eq!((t.listen.port(), t.cert.as_str()), (3443, "/c.pem"));
        assert!(parse(&format!("tls: {{cert: /c.pem, key: /k.pem}}\n{OK}")).is_err()); // listen wajib
        assert!(parse(&format!("tls: {{listen: '0.0.0.0:3443', cert: /c.pem}}\n{OK}")).is_err());
        assert!(parse(&format!("tls: {{listen: '0.0.0.0:3443', cert: '', key: /k.pem}}\n{OK}")).is_err());
        assert!(parse(&format!("tls: {{listen: '0.0.0.0:3443', cert: /c.pem, key: /k.pem, ca: x}}\n{OK}")).is_err());
        // bentrok dengan listen (OK memakai 0.0.0.0:3000)
        assert!(parse(&format!("tls: {{listen: '0.0.0.0:3000', cert: /c.pem, key: /k.pem}}\n{OK}")).is_err());
    }

    #[test]
    fn rejects_bad_health_values() {
        assert!(parse(&format!("health: {{interval_secs: 0}}\n{OK}")).is_err());
        assert!(parse(&OK.replace("  - path: /api\n", "  - path: /api\n    health_path: health\n")).is_err());
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

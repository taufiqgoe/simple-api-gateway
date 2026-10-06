use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Duration;

/// Batas atas bucket durasi (detik); bucket terakhir adalah +Inf.
pub const BUCKETS: [f64; 10] = [0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0];

/// Counter per upstream (tanpa lock). Durasi diukur sampai header respons diterima.
#[derive(Default)]
pub struct Stats {
    pub requests: AtomicU64,
    /// 2xx, 3xx, 4xx, 5xx dari upstream.
    pub status: [AtomicU64; 4],
    pub err_connect: AtomicU64,
    pub err_timeout: AtomicU64,
    pub err_other: AtomicU64,
    /// Request yang mendarat di upstream ini sebagai percobaan ulang setelah upstream lain gagal connect.
    pub retried_in: AtomicU64,
    pub ejections: AtomicU64,
    hist: [AtomicU64; BUCKETS.len() + 1],
    sum_micros: AtomicU64,
}

impl Stats {
    pub fn observe(&self, status: u16, took: Duration) {
        if let 200..=599 = status {
            self.status[(status as usize / 100) - 2].fetch_add(1, Relaxed);
        }
        let secs = took.as_secs_f64();
        let bucket = BUCKETS.iter().position(|b| secs <= *b).unwrap_or(BUCKETS.len());
        self.hist[bucket].fetch_add(1, Relaxed);
        self.sum_micros.fetch_add(took.as_micros() as u64, Relaxed);
    }
}

pub struct Global {
    pub no_route: AtomicU64,
    pub reloads: AtomicU64,
    pub reload_failures: AtomicU64,
}

pub struct UpstreamView<'a> {
    pub route: &'a str,
    pub upstream: String,
    pub up: bool,
    pub stats: &'a Stats,
}

/// Format teks Prometheus.
pub fn render(views: &[UpstreamView], global: &Global) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "# TYPE gateway_requests_total counter");
    for v in views {
        let l = format!("route=\"{}\",upstream=\"{}\"", v.route, v.upstream);
        let _ = writeln!(o, "gateway_requests_total{{{l}}} {}", v.stats.requests.load(Relaxed));
    }
    let _ = writeln!(o, "# TYPE gateway_responses_total counter");
    for v in views {
        for (i, class) in ["2xx", "3xx", "4xx", "5xx"].iter().enumerate() {
            let _ = writeln!(
                o,
                "gateway_responses_total{{route=\"{}\",upstream=\"{}\",class=\"{class}\"}} {}",
                v.route,
                v.upstream,
                v.stats.status[i].load(Relaxed)
            );
        }
    }
    let _ = writeln!(o, "# TYPE gateway_upstream_errors_total counter");
    for v in views {
        for (kind, c) in [("connect", &v.stats.err_connect), ("timeout", &v.stats.err_timeout), ("other", &v.stats.err_other)] {
            let _ = writeln!(
                o,
                "gateway_upstream_errors_total{{route=\"{}\",upstream=\"{}\",kind=\"{kind}\"}} {}",
                v.route,
                v.upstream,
                c.load(Relaxed)
            );
        }
    }
    let _ = writeln!(o, "# TYPE gateway_retries_total counter");
    let _ = writeln!(o, "# TYPE gateway_ejections_total counter");
    let _ = writeln!(o, "# TYPE gateway_upstream_up gauge");
    for v in views {
        let l = format!("route=\"{}\",upstream=\"{}\"", v.route, v.upstream);
        let _ = writeln!(o, "gateway_retries_total{{{l}}} {}", v.stats.retried_in.load(Relaxed));
        let _ = writeln!(o, "gateway_ejections_total{{{l}}} {}", v.stats.ejections.load(Relaxed));
        let _ = writeln!(o, "gateway_upstream_up{{{l}}} {}", u8::from(v.up));
    }
    let _ = writeln!(o, "# TYPE gateway_request_duration_seconds histogram");
    for v in views {
        let l = format!("route=\"{}\",upstream=\"{}\"", v.route, v.upstream);
        let mut cumulative = 0u64;
        for (i, b) in BUCKETS.iter().enumerate() {
            cumulative += v.stats.hist[i].load(Relaxed);
            let _ = writeln!(o, "gateway_request_duration_seconds_bucket{{{l},le=\"{b}\"}} {cumulative}");
        }
        cumulative += v.stats.hist[BUCKETS.len()].load(Relaxed);
        let _ = writeln!(o, "gateway_request_duration_seconds_bucket{{{l},le=\"+Inf\"}} {cumulative}");
        let _ = writeln!(o, "gateway_request_duration_seconds_sum{{{l}}} {}", v.stats.sum_micros.load(Relaxed) as f64 / 1e6);
        let _ = writeln!(o, "gateway_request_duration_seconds_count{{{l}}} {cumulative}");
    }
    let _ = writeln!(o, "# TYPE gateway_no_route_total counter\ngateway_no_route_total {}", global.no_route.load(Relaxed));
    let _ = writeln!(o, "# TYPE gateway_config_reloads_total counter\ngateway_config_reloads_total {}", global.reloads.load(Relaxed));
    let _ = writeln!(
        o,
        "# TYPE gateway_config_reload_failures_total counter\ngateway_config_reload_failures_total {}",
        global.reload_failures.load(Relaxed)
    );
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observe_buckets_and_status_classes() {
        let s = Stats::default();
        s.observe(200, Duration::from_millis(3));
        s.observe(503, Duration::from_millis(300));
        s.observe(404, Duration::from_secs(9));
        assert_eq!(s.status[0].load(Relaxed), 1);
        assert_eq!(s.status[2].load(Relaxed), 1);
        assert_eq!(s.status[3].load(Relaxed), 1);
        let g = Global { no_route: AtomicU64::new(0), reloads: AtomicU64::new(0), reload_failures: AtomicU64::new(0) };
        let out = render(&[UpstreamView { route: "/", upstream: "a:1".into(), up: true, stats: &s }], &g);
        assert!(out.contains("gateway_request_duration_seconds_bucket{route=\"/\",upstream=\"a:1\",le=\"0.005\"} 1"), "{out}");
        assert!(out.contains("le=\"+Inf\"} 3"), "{out}");
        assert!(out.contains("gateway_upstream_up{route=\"/\",upstream=\"a:1\"} 1"), "{out}");
    }
}

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct PrincipalId(pub(crate) Arc<str>);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct RequestKey {
    method: &'static str,
    route: &'static str,
    principal: String,
    status: u16,
}

#[derive(Clone, Copy, Debug)]
struct RequestSeries {
    total: u64,
    duration_seconds_sum: f64,
}

#[derive(Default)]
pub(crate) struct RequestMetrics {
    series: Mutex<HashMap<RequestKey, RequestSeries>>,
}

impl RequestMetrics {
    pub(crate) fn record(
        &self,
        method: &str,
        route: &str,
        principal: Option<&str>,
        status: u16,
        duration: Duration,
    ) {
        let key = RequestKey {
            method: metric_method(method),
            route: metric_route(route),
            principal: principal.unwrap_or("anonymous").to_owned(),
            status,
        };
        let value = RequestSeries {
            total: 1,
            duration_seconds_sum: duration.as_secs_f64(),
        };
        let Ok(mut series) = self.series.lock() else {
            return;
        };
        let entry = series.entry(key).or_insert(RequestSeries {
            total: 0,
            duration_seconds_sum: 0.0,
        });
        entry.total += value.total;
        entry.duration_seconds_sum += value.duration_seconds_sum;
    }

    pub(crate) fn render(&self, backend: &str, active_sandboxes: usize) -> String {
        let mut output = String::from(
            "# HELP sandbox_gateway_requests_total Total gateway requests.\n\
             # TYPE sandbox_gateway_requests_total counter\n",
        );
        let Ok(series) = self.series.lock() else {
            return output;
        };
        let mut series = series.iter().collect::<Vec<_>>();
        series.sort_by(|left, right| {
            left.0
                .method
                .cmp(right.0.method)
                .then(left.0.route.cmp(right.0.route))
                .then(left.0.principal.cmp(&right.0.principal))
                .then(left.0.status.cmp(&right.0.status))
        });
        for (key, value) in series {
            let labels = format!(
                "method={:?},route={:?},principal={:?},status={:?}",
                key.method,
                key.route,
                key.principal,
                key.status.to_string()
            );
            output.push_str(&format!(
                "sandbox_gateway_requests_total{{{labels}}} {}\n",
                value.total
            ));
            output.push_str(&format!(
                "sandbox_gateway_request_duration_seconds_sum{{{labels}}} {:.6}\n",
                value.duration_seconds_sum
            ));
            output.push_str(&format!(
                "sandbox_gateway_request_duration_seconds_count{{{labels}}} {}\n",
                value.total
            ));
        }
        output.push_str(
            "# HELP sandbox_gateway_active_sandboxes Current sandbox records in this gateway.\n\
             # TYPE sandbox_gateway_active_sandboxes gauge\n",
        );
        output.push_str(&format!(
            "sandbox_gateway_active_sandboxes{{backend={backend:?}}} {active_sandboxes}\n"
        ));
        output
    }
}

#[derive(Default)]
pub(crate) struct RateLimiter {
    windows: Mutex<HashMap<String, RateWindow>>,
}

#[derive(Clone, Copy)]
struct RateWindow {
    started_at: Instant,
    requests: usize,
}

impl RateLimiter {
    pub(crate) fn check(
        &self,
        principal: &str,
        requests_per_minute: Option<usize>,
    ) -> Option<Duration> {
        let limit = requests_per_minute?;
        let mut windows = self.windows.lock().ok()?;
        let now = Instant::now();
        let window = windows.entry(principal.to_owned()).or_insert(RateWindow {
            started_at: now,
            requests: 0,
        });
        if window.started_at.elapsed() >= Duration::from_secs(60) {
            *window = RateWindow {
                started_at: now,
                requests: 0,
            };
        }
        if window.requests < limit {
            window.requests += 1;
            return None;
        }
        let elapsed = window.started_at.elapsed();
        let remaining = 60u64.saturating_sub(elapsed.as_secs());
        Some(Duration::from_secs(remaining.max(1)))
    }
}

fn metric_method(method: &str) -> &'static str {
    match method {
        "DELETE" => "DELETE",
        "GET" => "GET",
        "POST" => "POST",
        "PUT" => "PUT",
        "PATCH" => "PATCH",
        _ => "OTHER",
    }
}

pub(crate) fn metric_route(path: &str) -> &'static str {
    match path {
        "/healthz" => return "/healthz",
        "/readyz" => return "/readyz",
        "/metrics" => return "/metrics",
        "/v1/sandboxes" => return "/v1/sandboxes",
        _ => {}
    }
    let Some(rest) = path.strip_prefix("/v1/sandboxes/") else {
        return "other";
    };
    let mut parts = rest.split('/');
    if parts.next().is_none() {
        return "other";
    }
    if parts.next().is_none() {
        return "/v1/sandboxes/{id}";
    }
    let Some(action) = parts.next() else {
        return "/v1/sandboxes/{id}/{action}";
    };
    match action {
        "fs" => "/v1/sandboxes/{id}/fs/{action}",
        "exec" => "/v1/sandboxes/{id}/exec/{action}",
        _ => "/v1/sandboxes/{id}/{action}",
    }
}

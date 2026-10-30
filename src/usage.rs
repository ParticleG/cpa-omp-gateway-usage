//! Read-only Gateway quota client and its validated plugin configuration.

use std::io::Read as _;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use ureq::Agent;
use ureq::tls::{RootCerts, TlsConfig};
use url::Url;

const DEFAULT_GATEWAY_URL: &str = "http://omp-gateway:4000";
const DEFAULT_TOKEN_FILE: &str = "/run/secrets/omp-gateway-usage.token";
const MAX_RESPONSE_BYTES: usize = 2 << 20;

const ERR_INVALID_YAML: &str = "invalid plugin configuration YAML";
const ERR_ORIGIN: &str = "gateway_url must be an HTTP(S) origin without credentials, query or path";
const ERR_TOKEN_FILE: &str = "token_file must be an absolute server-side path";

const ERR_TOKEN_UNAVAILABLE: &str =
    "Gateway token file is unavailable; check the server-side mount.";
const ERR_TOKEN_INVALID: &str = "Gateway token file is invalid.";
const ERR_CONFIG: &str = "Gateway configuration is invalid.";
const ERR_REQUEST: &str = "Gateway quota request failed or timed out.";
const ERR_RESPONSE: &str = "Gateway quota response is unreadable or too large.";
const ERR_SCHEMA: &str = "Gateway returned an unsupported quota response.";

pub(crate) struct Config {
    pub(crate) gateway_url: String,
    pub(crate) token_file: String,
}

impl Config {
    pub(crate) fn from_yaml(raw: &[u8]) -> Result<Self, &'static str> {
        let parsed: Option<ConfigYaml> = if raw.iter().all(|byte| byte.is_ascii_whitespace()) {
            None
        } else {
            serde_yaml_ng::from_slice(raw).map_err(|_| ERR_INVALID_YAML)?
        };
        let (gateway_url, token_file) = match parsed {
            Some(value) => (
                value
                    .gateway_url
                    .unwrap_or_else(|| DEFAULT_GATEWAY_URL.to_owned()),
                value
                    .token_file
                    .unwrap_or_else(|| DEFAULT_TOKEN_FILE.to_owned()),
            ),
            None => (
                DEFAULT_GATEWAY_URL.to_owned(),
                DEFAULT_TOKEN_FILE.to_owned(),
            ),
        };
        let config = Self {
            gateway_url,
            token_file,
        };
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), &'static str> {
        if !valid_origin(&self.gateway_url) {
            return Err(ERR_ORIGIN);
        }
        if !Path::new(&self.token_file).is_absolute() {
            return Err(ERR_TOKEN_FILE);
        }
        Ok(())
    }
}

/// A Gateway usage response only accepts an HTTP(S) origin.
///
/// The raw string is checked as well as the parsed URL so that empty userinfo,
/// query/fragment delimiters, backslash normalization and dot-segment
/// normalization cannot turn a non-origin into an accepted origin.
fn valid_origin(value: &str) -> bool {
    if value.bytes().any(|byte| {
        byte.is_ascii_whitespace()
            || byte.is_ascii_control()
            || matches!(byte, b'\\' | b'@' | b'?' | b'#')
    }) {
        return false;
    }
    let Ok(parsed) = Url::parse(value) else {
        return false;
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        return false;
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return false;
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return false;
    }
    if parsed.path() != "/" {
        return false;
    }
    let Some((_, after_scheme)) = value.split_once("://") else {
        return false;
    };
    let authority_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    matches!(&after_scheme[authority_end..], "" | "/")
}

fn usage_url(config: &Config) -> String {
    format!("{}/v1/usage", config.gateway_url.trim_end_matches('/'))
}

#[derive(Clone)]
pub(crate) struct UsageClient {
    agent: Agent,
}

impl UsageClient {
    pub(crate) fn new() -> Self {
        let tls = TlsConfig::builder()
            .root_certs(RootCerts::PlatformVerifier)
            .build();
        let config = Agent::config_builder()
            // Never let an environment-selected proxy receive the Gateway token.
            .proxy(None)
            // Redirects are returned as responses, not followed with credentials.
            .max_redirects(0)
            .max_redirects_will_error(false)
            .http_status_as_error(false)
            .tls_config(tls)
            .timeout_global(Some(Duration::from_secs(12)))
            .build();
        Self {
            agent: Agent::new_with_config(config),
        }
    }

    pub(crate) fn fetch(&self, config: &Config) -> Result<UsageView, String> {
        let token = read_token(&config.token_file)?;
        let target = Url::parse(&usage_url(config)).map_err(|_| ERR_CONFIG.to_owned())?;
        let mut response = self
            .agent
            .get(target.as_str())
            .header("Authorization", format!("Bearer {}", token.trim()))
            .header("Accept", "application/json")
            .call()
            .map_err(|_| ERR_REQUEST.to_owned())?;
        let status = response.status().as_u16();
        if status != 200 {
            return Err(format!(
                "Gateway quota request returned HTTP {status}; check Gateway health and its main token."
            ));
        }
        // The limit applies to the decoded body, matching the old transport's
        // LimitReader around net/http's transparent gzip reader.
        let mut reader = response
            .body_mut()
            .as_reader()
            .take(MAX_RESPONSE_BYTES as u64 + 1);
        let mut body = Vec::new();
        reader
            .read_to_end(&mut body)
            .map_err(|_| ERR_RESPONSE.to_owned())?;
        if body.len() > MAX_RESPONSE_BYTES {
            return Err(ERR_RESPONSE.to_owned());
        }
        let upstream: GatewayUsage =
            serde_json::from_slice(&body).map_err(|_| ERR_SCHEMA.to_owned())?;
        if !matches!(upstream.generated_at, Some(value) if value > 0) || upstream.reports.is_none()
        {
            return Err(ERR_SCHEMA.to_owned());
        }
        sanitize_usage(upstream).map_err(str::to_owned)
    }
}

fn read_token(path: &str) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|_| ERR_TOKEN_UNAVAILABLE.to_owned())?;
    let contents = String::from_utf8(bytes).map_err(|_| ERR_TOKEN_INVALID.to_owned())?;
    let token = contents.trim();
    if token.is_empty() || token.contains('\r') || token.contains('\n') {
        return Err(ERR_TOKEN_INVALID.to_owned());
    }
    Ok(contents)
}

#[derive(Deserialize)]
struct ConfigYaml {
    gateway_url: Option<String>,
    token_file: Option<String>,
}

#[derive(Deserialize)]
struct GatewayUsage {
    #[serde(rename = "generatedAt")]
    generated_at: Option<i64>,
    reports: Option<Vec<Option<GatewayReport>>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GatewayReport {
    provider: Option<String>,
    fetched_at: Option<i64>,
    limits: Option<Vec<Option<GatewayLimit>>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GatewayLimit {
    label: Option<String>,
    window: Option<GatewayWindow>,
    amount: Option<GatewayAmount>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GatewayWindow {
    duration_ms: Option<i64>,
    resets_at: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GatewayAmount {
    unit: Option<String>,
    used: Option<f64>,
    limit: Option<f64>,
    used_fraction: Option<f64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UsageView {
    generated_at: i64,
    reports: Vec<ReportView>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReportView {
    provider: &'static str,
    fetched_at: i64,
    limits: Vec<LimitView>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LimitView {
    label: String,
    used_percent: Option<f64>,
    remaining_percent: Option<f64>,
    resets_at: i64,
    duration_ms: i64,
}

fn sanitize_usage(upstream: GatewayUsage) -> Result<UsageView, &'static str> {
    let GatewayUsage {
        generated_at,
        reports,
    } = upstream;
    let reports = reports.unwrap_or_default();
    let mut report_views = Vec::with_capacity(reports.len());
    for report in reports.into_iter().flatten() {
        if report.provider.as_deref() != Some("anthropic") {
            continue;
        }
        let limits = report.limits.unwrap_or_default();
        let mut limit_views = Vec::with_capacity(limits.len());
        for limit in limits.into_iter().flatten() {
            let Some(amount) = limit.amount.as_ref() else {
                continue;
            };
            if amount.unit.as_deref() != Some("percent") {
                continue;
            }
            let used = match amount.used_fraction {
                Some(fraction) => Some(fraction * 100.0),
                None => match (amount.used, amount.limit) {
                    (Some(used), Some(capacity)) if capacity > 0.0 => Some(used / capacity * 100.0),
                    _ => None,
                },
            };
            // Invalid arithmetic is an upstream error, not missing quota.
            if used.is_some_and(|value| !value.is_finite()) {
                return Err(ERR_SCHEMA);
            }
            let used_percent = used.filter(|value| *value >= 0.0);
            let remaining_percent = used_percent.map(|value| (100.0 - value).max(0.0));
            let window = limit.window.as_ref();
            limit_views.push(LimitView {
                label: limit.label.unwrap_or_default(),
                used_percent,
                remaining_percent,
                resets_at: window.and_then(|window| window.resets_at).unwrap_or(0),
                duration_ms: window.and_then(|window| window.duration_ms).unwrap_or(0),
            });
        }
        report_views.push(ReportView {
            provider: "anthropic",
            fetched_at: report.fetched_at.unwrap_or(0),
            limits: limit_views,
        });
    }
    Ok(UsageView {
        generated_at: generated_at.unwrap_or(0),
        reports: report_views,
    })
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    use super::*;

    #[derive(Clone)]
    struct RecordedRequest {
        method: String,
        path: String,
        authorization: Option<String>,
        accept: Option<String>,
    }

    struct TestServer {
        address: SocketAddr,
        stop: Arc<AtomicBool>,
        requests: Arc<Mutex<Vec<RecordedRequest>>>,
        connections: Arc<AtomicUsize>,
        handle: Option<JoinHandle<()>>,
    }

    impl TestServer {
        fn url(&self) -> String {
            format!("http://{}", self.address)
        }

        fn requests(&self) -> Vec<RecordedRequest> {
            self.requests.lock().expect("request log").clone()
        }

        fn connections(&self) -> usize {
            self.connections.load(Ordering::SeqCst)
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(handle) = self.handle.take() {
                let result = handle.join();
                if !thread::panicking() {
                    result.expect("HTTP fixture thread panicked");
                }
            }
        }
    }

    fn spawn_server<F>(handler: F) -> TestServer
    where
        F: Fn(&RecordedRequest) -> Vec<u8> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let address = listener.local_addr().expect("listener address");
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let worker_stop = Arc::clone(&stop);
        let worker_requests = Arc::clone(&requests);
        let worker_connections = Arc::clone(&connections);
        let handle = thread::spawn(move || {
            while !worker_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        worker_connections.fetch_add(1, Ordering::SeqCst);
                        stream.set_nonblocking(false).expect("blocking connection");
                        stream
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .expect("read timeout");
                        stream
                            .set_write_timeout(Some(Duration::from_secs(2)))
                            .expect("write timeout");
                        let Some(request) = read_request(&mut stream) else {
                            continue;
                        };
                        worker_requests
                            .lock()
                            .expect("request log")
                            .push(request.clone());
                        let response = handler(&request);
                        let _ = stream.write_all(&response);
                        let _ = stream.flush();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        TestServer {
            address,
            stop,
            requests,
            connections,
            handle: Some(handle),
        }
    }

    fn read_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
        let mut data = Vec::new();
        let mut buffer = [0_u8; 2048];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    data.extend_from_slice(&buffer[..count]);
                    if data.windows(4).any(|window| window == &b"\r\n\r\n"[..])
                        || data.len() >= 64 * 1024
                    {
                        break;
                    }
                }
                Err(_) => return None,
            }
        }
        let text = String::from_utf8_lossy(&data);
        let mut lines = text.split("\r\n");
        let request_line = lines.next()?;
        let mut parts = request_line.split_whitespace();
        let method = parts.next()?.to_owned();
        let path = parts.next()?.to_owned();
        let mut authorization = None;
        let mut accept = None;
        for line in lines {
            if line.is_empty() {
                break;
            }
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            if name.eq_ignore_ascii_case("authorization") {
                authorization = Some(value.trim().to_owned());
            } else if name.eq_ignore_ascii_case("accept") {
                accept = Some(value.trim().to_owned());
            }
        }
        Some(RecordedRequest {
            method,
            path,
            authorization,
            accept,
        })
    }

    fn http_response(status: u16, reason: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        for &(name, value) in headers {
            response.push_str(name);
            response.push_str(": ");
            response.push_str(value);
            response.push_str("\r\n");
        }
        response.push_str("\r\n");
        let mut bytes = response.into_bytes();
        bytes.extend_from_slice(body);
        bytes
    }

    fn reason_for(status: u16) -> &'static str {
        match status {
            200 => "OK",
            307 => "Temporary Redirect",
            403 => "Forbidden",
            _ => "Test",
        }
    }

    fn token_config(endpoint: &str) -> (tempfile::TempDir, Config) {
        token_config_with(endpoint, b"test-gateway-secret\n")
    }

    fn token_config_with(endpoint: &str, token: &[u8]) -> (tempfile::TempDir, Config) {
        let directory = tempfile::tempdir().expect("temporary token directory");
        let token_file = directory.path().join("token");
        std::fs::write(&token_file, token).expect("write token");
        let config = Config {
            gateway_url: endpoint.to_owned(),
            token_file: token_file.to_str().expect("utf-8 token path").to_owned(),
        };
        (directory, config)
    }

    #[test]
    fn quota_projection_removes_identity_and_non_claude_reports() {
        const BODY: &str = r#"{"generatedAt":1793010268749,"metadata":{"token":"secret-sentinel"},"reports":[{"provider":"anthropic","fetchedAt":1793010268169,"metadata":{"email":"private@example.test","accountId":"private-account"},"resetCredits":{"reason":"secret-sentinel"},"limits":[{"id":"anthropic:5h","label":"Claude 5 Hour","window":{"durationMs":18000000,"resetsAt":1793026800059},"amount":{"unit":"percent","usedFraction":0.12,"used":99,"limit":100},"metadata":{"secret":"secret-sentinel"}},{"label":"Claude Extra Usage","amount":{"unit":"USD","used":2,"limit":5}}]},{"provider":"openai-codex","limits":[{"label":"private-codex-account"}]}]}"#;
        let body = BODY.as_bytes().to_vec();
        let server = spawn_server(move |_| http_response(200, "OK", &[], &body));
        let (_directory, config) = token_config(&format!("{}/", server.url()));

        let view =
            serde_json::to_value(UsageClient::new().fetch(&config).expect("quota projection"))
                .expect("serialize projected quota");

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].path, "/v1/usage");
        assert_eq!(
            requests[0].authorization.as_deref(),
            Some("Bearer test-gateway-secret")
        );
        assert_eq!(requests[0].accept.as_deref(), Some("application/json"));

        assert_eq!(view["generatedAt"].as_i64(), Some(1_793_010_268_749));
        let reports = view["reports"].as_array().expect("reports array");
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0]["provider"].as_str(), Some("anthropic"));
        assert_eq!(reports[0]["fetchedAt"].as_i64(), Some(1_793_010_268_169));
        let limits = reports[0]["limits"].as_array().expect("limits array");
        assert_eq!(limits.len(), 1);
        let limit = &limits[0];
        assert_eq!(limit["label"].as_str(), Some("Claude 5 Hour"));
        assert_eq!(limit["usedPercent"].as_f64(), Some(12.0));
        assert_eq!(limit["remainingPercent"].as_f64(), Some(88.0));
        assert_eq!(limit["resetsAt"].as_i64(), Some(1_793_026_800_059));
        assert_eq!(limit["durationMs"].as_i64(), Some(18_000_000));

        let serialized = view.to_string();
        for private in [
            "secret-sentinel",
            "private@example.test",
            "private-account",
            "private-codex-account",
            "test-gateway-secret",
            "metadata",
            "resetCredits",
        ] {
            assert!(
                !serialized.contains(private),
                "private field leaked: {private}"
            );
        }
    }

    #[test]
    fn quota_unknown_zero_and_exhausted_remain_distinct() {
        let input: GatewayUsage = serde_json::from_slice(
            br#"{"reports":[{"provider":"anthropic","limits":[{"amount":{"unit":"percent"}},{"amount":{"unit":"percent","used":0,"limit":100}},{"amount":{"unit":"percent","used":110,"limit":100}},{"amount":{"unit":"percent","used":0,"limit":0}}]}]}"#,
        )
        .expect("usage fixture");
        let view = sanitize_usage(input).expect("finite quota");
        let limits = &view.reports[0].limits;
        assert_eq!(limits.len(), 4);
        assert!(limits[0].used_percent.is_none());
        assert!(limits[0].remaining_percent.is_none());
        assert_eq!(limits[1].used_percent, Some(0.0));
        assert_eq!(limits[1].remaining_percent, Some(100.0));
        assert!(
            limits[2]
                .used_percent
                .is_some_and(|used| (used - 110.0).abs() < 1e-9)
        );
        assert_eq!(limits[2].remaining_percent, Some(0.0));
        assert!(limits[3].used_percent.is_none());
        assert!(limits[3].remaining_percent.is_none());

        let serialized = serde_json::to_value(&view).expect("serialize usage");
        assert!(serialized["reports"][0]["limits"][0]["usedPercent"].is_null());
        assert!(serialized["reports"][0]["limits"][0]["remainingPercent"].is_null());
    }

    #[test]
    fn null_nested_quota_fields_use_zero_values() {
        let input: GatewayUsage = serde_json::from_slice(
            br#"{"generatedAt":42,"reports":[null,{"provider":"anthropic","fetchedAt":null,"limits":null},{"provider":"anthropic","limits":[null,{"label":null,"window":null,"amount":null},{"label":null,"window":{"durationMs":null,"resetsAt":null},"amount":{"unit":"percent","used":0,"limit":100}}]},{"provider":"anthropic"}]}"#,
        )
        .expect("null usage fixture");
        let view = sanitize_usage(input).expect("nullable quota fields");
        assert_eq!(view.generated_at, 42);
        assert_eq!(view.reports.len(), 3);

        assert_eq!(view.reports[0].fetched_at, 0);
        assert!(view.reports[0].limits.is_empty());

        assert_eq!(view.reports[1].fetched_at, 0);
        assert_eq!(view.reports[1].limits.len(), 1);
        let limit = &view.reports[1].limits[0];
        assert_eq!(limit.label, "");
        assert_eq!(limit.resets_at, 0);
        assert_eq!(limit.duration_ms, 0);
        assert_eq!(limit.used_percent, Some(0.0));
        assert_eq!(limit.remaining_percent, Some(100.0));

        assert_eq!(view.reports[2].fetched_at, 0);
        assert!(view.reports[2].limits.is_empty());
    }

    #[test]
    fn quota_requires_generated_at_and_non_null_reports() {
        for body in [
            r#"{"generatedAt":1,"reports":null}"#,
            r#"{"generatedAt":1}"#,
            r#"{"generatedAt":null,"reports":[]}"#,
            r#"{"generatedAt":0,"reports":[]}"#,
            r#"{"generatedAt":-1,"reports":[]}"#,
            r#"{"reports":[]}"#,
        ] {
            let response_body = body.as_bytes().to_vec();
            let server = spawn_server(move |_| http_response(200, "OK", &[], &response_body));
            let (_directory, config) = token_config(&server.url());
            assert!(
                UsageClient::new().fetch(&config).is_err(),
                "invalid response was accepted: {body}"
            );
        }
    }

    #[test]
    fn quota_rejects_redirects_without_forwarding_token() {
        let destination = spawn_server(|_| http_response(200, "OK", &[], b"{}"));
        let location = destination.url();
        let source = spawn_server(move |_| {
            http_response(
                307,
                "Temporary Redirect",
                &[("Location", location.as_str())],
                b"",
            )
        });
        let (_directory, config) = token_config(&source.url());

        let error = UsageClient::new()
            .fetch(&config)
            .expect_err("redirect must fail");
        assert!(error.contains("307"), "unexpected error: {error}");
        assert_eq!(
            source.requests()[0].authorization.as_deref(),
            Some("Bearer test-gateway-secret")
        );
        assert_eq!(destination.connections(), 0);
        assert!(destination.requests().is_empty());
    }

    #[test]
    fn quota_errors_never_expose_upstream_bodies() {
        let cases: [(&str, u16, Vec<u8>); 4] = [
            (
                "denied",
                403,
                br#"{"error":"test-gateway-secret private@example.test"}"#.to_vec(),
            ),
            (
                "malformed",
                200,
                b"test-gateway-secret private@example.test".to_vec(),
            ),
            ("wrong-schema", 200, b"{}".to_vec()),
            ("oversized", 200, vec![b'x'; MAX_RESPONSE_BYTES + 1]),
        ];
        for (name, status, body) in cases {
            let server =
                spawn_server(move |_| http_response(status, reason_for(status), &[], &body));
            let (_directory, config) = token_config(&server.url());
            let error = UsageClient::new().fetch(&config).expect_err(name);
            assert!(!error.contains("test-gateway-secret"), "{name}: {error}");
            assert!(!error.contains("private@example.test"), "{name}: {error}");
            if status != 200 {
                assert!(error.contains(&status.to_string()), "{name}: {error}");
            }
        }
    }

    #[test]
    fn token_file_errors_are_sanitized_and_do_not_contact_gateway() {
        let server = spawn_server(|_| http_response(200, "OK", &[], b"{}"));
        let directory = tempfile::tempdir().expect("temporary token directory");
        let missing = directory.path().join("server-secret-token-name");
        let missing_config = Config {
            gateway_url: server.url(),
            token_file: missing
                .to_str()
                .expect("utf-8 missing token path")
                .to_owned(),
        };
        let error = UsageClient::new()
            .fetch(&missing_config)
            .expect_err("missing token file");
        assert!(!error.contains("server-secret-token-name"));

        let token_file = directory.path().join("token");
        let invalid_config = Config {
            gateway_url: server.url(),
            token_file: token_file.to_str().expect("utf-8 token path").to_owned(),
        };
        let invalid_tokens: [&[u8]; 4] = [b"", b" \n\t", b"bad\r\ntoken", b"\xff"];
        for token in invalid_tokens {
            std::fs::write(&token_file, token).expect("write invalid token");
            assert!(UsageClient::new().fetch(&invalid_config).is_err());
        }
        assert_eq!(server.connections(), 0);
    }

    #[test]
    fn token_is_read_for_each_request() {
        let body = br#"{"generatedAt":1,"reports":[]}"#.to_vec();
        let server = spawn_server(move |_| http_response(200, "OK", &[], &body));
        let (_directory, config) = token_config_with(&server.url(), b"first-token\n");
        let client = UsageClient::new();

        client.fetch(&config).expect("first quota fetch");
        std::fs::write(config.token_file.as_str(), b"second-token\n").expect("rotate token");
        client.fetch(&config).expect("second quota fetch");

        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].authorization.as_deref(),
            Some("Bearer first-token")
        );
        assert_eq!(
            requests[1].authorization.as_deref(),
            Some("Bearer second-token")
        );
    }

    #[test]
    fn gateway_origins_reject_credentials_and_paths() {
        let directory = tempfile::tempdir().expect("temporary token directory");
        let token = directory.path().join("token");

        let unsafe_origins = [
            "https://user:password@example.test",
            "https://@example.test",
            "https://example.test?token=secret",
            "https://example.test/v1/usage",
            "https://example.test/#fragment",
            "http://example.test/..",
            "file:///tmp/token",
            "http://",
        ];
        for endpoint in unsafe_origins {
            let raw = serde_json::to_vec(&serde_json::json!({
                "gateway_url": endpoint,
                "token_file": token.to_str().expect("UTF-8 token path"),
            }))
            .expect("serialize configuration");
            let error = Config::from_yaml(&raw)
                .err()
                .expect("unsafe origin must be rejected");
            assert!(!error.contains(endpoint));
        }
    }

    #[test]
    fn token_file_must_be_absolute() {
        for token in ["relative/token", "", "./token", "~/.token"] {
            let raw = serde_json::to_vec(&serde_json::json!({
                "gateway_url": "http://omp-gateway:4000",
                "token_file": token,
            }))
            .expect("serialize configuration");
            assert!(
                Config::from_yaml(&raw).is_err(),
                "relative token path accepted: {token}"
            );
        }
    }

    #[test]
    fn overflowing_quota_is_an_error_not_missing_data() {
        for amount in [
            r#"{"unit":"percent","usedFraction":1e308}"#,
            r#"{"unit":"percent","used":1e308,"limit":1e-308}"#,
        ] {
            let body = format!(
                r#"{{"generatedAt":1,"reports":[{{"provider":"anthropic","limits":[{{"amount":{amount}}}]}}]}}"#
            )
            .into_bytes();
            let server = spawn_server(move |_| http_response(200, "OK", &[], &body));
            let (_directory, config) = token_config(&server.url());
            assert!(UsageClient::new().fetch(&config).is_err());
        }
    }
}

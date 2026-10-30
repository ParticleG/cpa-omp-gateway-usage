use std::borrow::Cow;
use std::sync::{Arc, Mutex, MutexGuard};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::usage::{Config, UsageClient};

#[cfg(test)]
const RESOURCE_PATH: &str = "/v0/resource/plugins/omp-gateway-usage";
const STATUS_PATH: &str = "/v0/resource/plugins/omp-gateway-usage/status";
const APP_JS_PATH: &str = "/v0/resource/plugins/omp-gateway-usage/app.js";
const STYLE_CSS_PATH: &str = "/v0/resource/plugins/omp-gateway-usage/style.css";
const USAGE_PATH: &str = "/v0/management/plugins/omp-gateway-usage/usage";

const INDEX_HTML: &[u8] = include_bytes!("../web/index.html");
const APP_JS: &[u8] = include_bytes!("../web/app.js");
const STYLE_CSS: &[u8] = include_bytes!("../web/style.css");

const JSON_CONTENT_TYPE: &str = "application/json; charset=utf-8";
const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; base-uri 'none'; frame-ancestors 'self'; form-action 'none'";
const UNCONFIGURED_ERROR: &str = "Gateway token file is unavailable; check the server-side mount.";
const ENCODING_FALLBACK: &[u8] =
    br#"{"ok":false,"error":{"code":"encoding_error","message":"cannot encode plugin response"}}"#;

struct Runtime {
    config: Config,
    client: UsageClient,
}

pub(crate) struct Plugin {
    state: Mutex<Option<Arc<Runtime>>>,
}

impl Plugin {
    pub(crate) const fn new() -> Self {
        Self {
            state: Mutex::new(None),
        }
    }

    pub(crate) fn dispatch(&self, method: &str, raw: &[u8]) -> Vec<u8> {
        match self.handle(method, raw) {
            Ok(result) => encode_envelope(Envelope {
                ok: true,
                result: Some(result),
                error: None,
            }),
            Err(message) => encode_envelope(Envelope {
                ok: false,
                result: None,
                error: Some(RpcError {
                    code: "plugin_error",
                    message,
                }),
            }),
        }
    }

    pub(crate) fn shutdown(&self) {
        let mut state = self.state();
        let _ = state.take();
    }

    fn handle(&self, method: &str, raw: &[u8]) -> Result<Value, &'static str> {
        match method {
            "plugin.register" | "plugin.reconfigure" => self.configure(raw),
            "management.register" => Ok(management_registration()),
            "management.handle" => handle_management(self, raw),
            _ => Err("unsupported plugin method"),
        }
    }

    fn configure(&self, raw: &[u8]) -> Result<Value, &'static str> {
        let (schema_version, config_yaml) = parse_registration(raw)?;
        if schema_version < 6 {
            return Err("plugin requires CPA RPC schema 6 or later");
        }
        let config = Config::from_yaml(config_yaml.as_deref().unwrap_or(&[]))?;
        // Validation completed before this point, so a rejected reconfiguration never
        // replaces the last usable configuration or its pooled client.
        let mut state = self.state();
        let client = state
            .as_ref()
            .map_or_else(UsageClient::new, |runtime| runtime.client.clone());
        *state = Some(Arc::new(Runtime { config, client }));
        drop(state);
        Ok(registration_metadata())
    }

    fn state(&self) -> MutexGuard<'_, Option<Arc<Runtime>>> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn snapshot(&self) -> Option<Arc<Runtime>> {
        self.state().as_ref().cloned()
    }
}

#[derive(Serialize)]
struct Envelope<'a> {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError<'a>>,
}

#[derive(Serialize)]
struct RpcError<'a> {
    code: &'static str,
    message: &'a str,
}

fn encode_envelope(envelope: Envelope<'_>) -> Vec<u8> {
    serde_json::to_vec(&envelope).unwrap_or_else(|_| ENCODING_FALLBACK.to_vec())
}

#[derive(Default, Deserialize)]
struct Registration<'a> {
    #[serde(default)]
    schema_version: u32,
    #[serde(borrow)]
    config_yaml: Option<&'a str>,
}

fn parse_registration(raw: &[u8]) -> Result<(u32, Option<Vec<u8>>), &'static str> {
    const INVALID: &str = "invalid plugin configuration envelope";
    let request: Option<Registration<'_>> = serde_json::from_slice(raw).map_err(|_| INVALID)?;
    let request = request.unwrap_or_default();
    let yaml = request
        .config_yaml
        .map(|encoded| STANDARD.decode(encoded))
        .transpose()
        .map_err(|_| INVALID)?;
    Ok((request.schema_version, yaml))
}

#[derive(Default, Deserialize)]
struct ManagementRequest<'a> {
    #[serde(rename = "Method", borrow, default)]
    method: Cow<'a, str>,
    #[serde(rename = "Path", borrow, default)]
    path: Cow<'a, str>,
}

fn parse_management_request(raw: &[u8]) -> Result<ManagementRequest<'_>, &'static str> {
    serde_json::from_slice::<Option<ManagementRequest<'_>>>(raw)
        .map(Option::unwrap_or_default)
        .map_err(|_| "invalid management request")
}

fn handle_management(plugin: &Plugin, raw: &[u8]) -> Result<Value, &'static str> {
    let request = parse_management_request(raw)?;
    if request.method != "GET" {
        let mut response = json_response(405, json!({"error": "Only GET is supported."}));
        response["Headers"]["Allow"] = json!(["GET"]);
        return Ok(response);
    }
    // Resource routes are public. Only this exact, CPA-authenticated route can read a
    // token or contact the Gateway.
    if request.path == USAGE_PATH {
        let Some(runtime) = plugin.snapshot() else {
            return Ok(json_response(502, json!({"error": UNCONFIGURED_ERROR})));
        };
        return Ok(match runtime.client.fetch(&runtime.config) {
            Ok(report) => json_response(200, report),
            Err(error) => json_response(502, json!({"error": error})),
        });
    }
    match request.path.as_ref() {
        STATUS_PATH => Ok(resource_response(INDEX_HTML, "text/html; charset=utf-8")),
        APP_JS_PATH => Ok(resource_response(APP_JS, "text/javascript; charset=utf-8")),
        STYLE_CSS_PATH => Ok(resource_response(STYLE_CSS, "text/css; charset=utf-8")),
        _ => Ok(json_response(404, json!({"error": "Not found."}))),
    }
}

fn json_response(status: u16, value: impl Serialize) -> Value {
    let (status, body) = match serde_json::to_vec(&value) {
        Ok(body) => (status, body),
        Err(_) => (500, br#"{"error":"Cannot encode usage data."}"#.to_vec()),
    };
    json!({
        "StatusCode": status,
        "Headers": response_headers(JSON_CONTENT_TYPE),
        "Body": STANDARD.encode(body),
    })
}

fn resource_response(body: &[u8], content_type: &str) -> Value {
    let mut headers = response_headers(content_type);
    headers["Content-Security-Policy"] = json!([CONTENT_SECURITY_POLICY]);
    json!({
        "StatusCode": 200,
        "Headers": headers,
        "Body": STANDARD.encode(body),
    })
}

fn response_headers(content_type: &str) -> Value {
    json!({
        "Content-Type": [content_type],
        "Cache-Control": ["no-store"],
        "X-Content-Type-Options": ["nosniff"],
        "Referrer-Policy": ["no-referrer"],
    })
}

fn registration_metadata() -> Value {
    json!({
        "schema_version": 6,
        "metadata": {
            "Name": "OMP Gateway Usage",
            "Version": crate::VERSION,
            "Author": "ParticleG",
            "GitHubRepository": "https://github.com/ParticleG/cpa-omp-gateway-usage",
            "ConfigFields": [
                {
                    "Name": "gateway_url",
                    "Type": "string",
                    "Description": "Private OMP Gateway origin; no credentials or path."
                },
                {
                    "Name": "token_file",
                    "Type": "string",
                    "Description": "Read-only server-side file containing the Gateway main token."
                }
            ]
        },
        "capabilities": {"management_api": true}
    })
}

fn management_registration() -> Value {
    json!({
        "routes": [{"Method": "GET", "Path": USAGE_PATH}],
        "resources": [
            {
                "Path": "/status",
                "Menu": "OMP Gateway Usage",
                "Description": "Read-only Claude subscription quota from OMP Gateway."
            },
            {"Path": "/app.js"},
            {"Path": "/style.css"}
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configuration_yaml(gateway_url: &str) -> String {
        let token_file = std::env::temp_dir().join("omp-gateway-usage-test.token");
        serde_yaml_ng::to_string(&json!({
            "gateway_url": gateway_url,
            "token_file": token_file.to_str().expect("UTF-8 temporary path"),
        }))
        .expect("configuration YAML")
    }

    fn registration(yaml: &str, schema_version: u32) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "config_yaml": STANDARD.encode(yaml),
            "schema_version": schema_version,
        }))
        .expect("registration request")
    }

    fn envelope(plugin: &Plugin, method: &str, raw: &[u8]) -> Value {
        serde_json::from_slice(&plugin.dispatch(method, raw)).expect("JSON envelope")
    }

    fn management(plugin: &Plugin, method: &str, path: &str) -> Value {
        let request = serde_json::to_vec(&json!({"Method": method, "Path": path}))
            .expect("management request");
        let response = envelope(plugin, "management.handle", &request);
        assert_eq!(response["ok"], true);
        response["result"].clone()
    }

    #[test]
    fn rejected_registration_does_not_install_configuration_or_echo_input() {
        let plugin = Plugin::new();
        let config = configuration_yaml("http://127.0.0.1:9");
        for request in [
            registration(&config, 5),
            br#"{"config_yaml":"secret-sentinel!","schema_version":6}"#.to_vec(),
            br#"{"schema_version":"secret-sentinel"}"#.to_vec(),
        ] {
            let response = envelope(&plugin, "plugin.register", &request);
            assert_eq!(response["ok"], false);
            assert_eq!(response["error"]["code"], "plugin_error");
            assert!(response.get("result").is_none());
            assert!(!response.to_string().contains("secret-sentinel"));
            assert!(plugin.snapshot().is_none());
        }
    }

    #[test]
    fn rejected_reconfiguration_preserves_the_last_valid_configuration() {
        let plugin = Plugin::new();
        let config = configuration_yaml("http://127.0.0.1:9");
        assert_eq!(
            envelope(&plugin, "plugin.register", &registration(&config, 6))["ok"],
            true
        );
        let before = plugin.snapshot().expect("configured plugin");
        for invalid in [
            configuration_yaml("https://user:password@example.test"),
            "gateway_url: [unterminated".to_owned(),
        ] {
            let response = envelope(&plugin, "plugin.reconfigure", &registration(&invalid, 7));
            assert_eq!(response["ok"], false);
            let after = plugin.snapshot().expect("previous configuration retained");
            assert_eq!(after.config.gateway_url, before.config.gateway_url);
            assert_eq!(after.config.token_file, before.config.token_file);
        }
    }

    #[test]
    fn public_near_match_and_non_get_paths_cannot_read_quota() {
        let plugin = Plugin::new();
        for path in [
            format!("{RESOURCE_PATH}/usage"),
            format!("{RESOURCE_PATH}/status/usage"),
            format!("{RESOURCE_PATH}/../usage"),
            format!("{USAGE_PATH}/"),
        ] {
            assert_eq!(
                management(&plugin, "GET", &path)["StatusCode"],
                404,
                "{path}"
            );
        }
        for (method, path) in [
            ("POST", USAGE_PATH),
            ("DELETE", STATUS_PATH),
            ("HEAD", APP_JS_PATH),
        ] {
            let response = management(&plugin, method, path);
            assert_eq!(response["StatusCode"], 405, "{method} {path}");
            assert_eq!(response["Headers"]["Allow"], json!(["GET"]));
        }
    }

    #[test]
    fn shutdown_releases_runtime_after_the_last_in_flight_snapshot() {
        let plugin = Plugin::new();
        let config = configuration_yaml("http://127.0.0.1:9");
        assert_eq!(
            envelope(&plugin, "plugin.register", &registration(&config, 6))["ok"],
            true
        );
        let request = plugin.snapshot().expect("in-flight runtime");
        let lifetime = Arc::downgrade(&request);
        plugin.shutdown();
        assert!(plugin.snapshot().is_none());
        assert!(lifetime.upgrade().is_some());
        drop(request);
        assert!(lifetime.upgrade().is_none());
    }
}

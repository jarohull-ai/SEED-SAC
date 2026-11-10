//! Small, blocking OpenAI-compatible client for the L1 worker thread.
//!
//! The client deliberately owns its HTTP agent and has no global state. A
//! caller can therefore construct one per worker thread and keep credentials
//! out of logs and error messages.

use std::time::Duration;

use dlp::DLPFilter;
use serde::Deserialize;

const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:1234/v1";
const DEFAULT_MODEL: &str = "local-model";
const DEFAULT_MAX_TOKENS: u32 = 512;
const DEFAULT_TEMPERATURE: f32 = 0.2;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PROMPT_BYTES: usize = 16 * 1024;
const MAX_RESPONSE_BYTES: u64 = 256 * 1024;

/// Configuration for one L1 client.
#[derive(Debug, Clone)]
pub struct L1Config {
    /// OpenAI-compatible API base, e.g. `http://127.0.0.1:1234/v1`.
    pub endpoint: String,
    pub model: String,
    pub max_tokens: u32,
    pub temperature: f32,
    pub timeout: Duration,
    /// Optional bearer token. It is never included in errors or logs.
    pub api_key: Option<String>,
}

impl Default for L1Config {
    fn default() -> Self {
        Self {
            endpoint: DEFAULT_ENDPOINT.to_owned(),
            model: DEFAULT_MODEL.to_owned(),
            max_tokens: DEFAULT_MAX_TOKENS,
            temperature: DEFAULT_TEMPERATURE,
            timeout: DEFAULT_TIMEOUT,
            api_key: None,
        }
    }
}

/// A caller-safe L1 client. `complete` blocks only the calling worker thread.
#[derive(Debug)]
pub struct L1Client {
    config: L1Config,
    agent: ureq::Agent,
}

/// Typed failures suitable for a UI or worker result without exposing secrets.
#[derive(Debug)]
pub enum L1Error {
    EmptyPrompt,
    InvalidEndpoint,
    Offline,
    Timeout,
    HttpStatus(u16),
    InvalidResponse,
}

impl std::fmt::Display for L1Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyPrompt => formatter.write_str("L1 request is empty"),
            Self::InvalidEndpoint => formatter.write_str("L1 endpoint is invalid"),
            Self::Offline => formatter.write_str("L1 service is offline"),
            Self::Timeout => formatter.write_str("L1 request timed out"),
            Self::HttpStatus(code) => write!(formatter, "L1 service returned HTTP {code}"),
            Self::InvalidResponse => formatter.write_str("L1 response was invalid"),
        }
    }
}

impl std::error::Error for L1Error {}

/// Report of DLP redactions applied to a prompt.
#[derive(Debug, Clone, Default)]
pub struct DLPReport {
    pub redaction_count: usize,
    pub rules_applied: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: Message,
}

#[derive(Debug, Deserialize)]
struct Message {
    content: Option<String>,
}

impl L1Client {
    /// Build a client with a per-request timeout and no shared mutable state.
    pub fn new(config: L1Config) -> Result<Self, L1Error> {
        if !is_local_endpoint(config.endpoint.trim()) || config.model.trim().is_empty() {
            return Err(L1Error::InvalidEndpoint);
        }
        let agent = ureq::Agent::config_builder()
            .proxy(None)
            .max_redirects(0)
            .timeout_global(Some(config.timeout))
            .build()
            .new_agent();
        Ok(Self { config, agent })
    }

    /// Send one prompt to `/chat/completions` and return the first text choice.
    /// Send prompt to L1 with DLP sanitization. Returns (response, DLP report).
    pub fn complete(&self, prompt: &str) -> Result<(String, DLPReport), L1Error> {
        if prompt.trim().is_empty() {
            return Err(L1Error::EmptyPrompt);
        }
        if prompt.len() > MAX_PROMPT_BYTES {
            return Err(L1Error::InvalidResponse);
        }

        // Apply DLP sanitization
        let dlp_filter = DLPFilter::new();
        let (sanitized, violations) = dlp_filter.sanitize(prompt);

        let dlp_report = DLPReport {
            redaction_count: violations.len(),
            rules_applied: violations
                .iter()
                .map(|v| v.rule.label().to_owned())
                .collect(),
        };

        let url = format!(
            "{}/chat/completions",
            self.config.endpoint.trim_end_matches('/')
        );
        let body = serde_json::json!({
            "model": self.config.model,
            "messages": [{"role": "user", "content": sanitized}],
            "max_tokens": self.config.max_tokens,
            "temperature": self.config.temperature,
            "stream": false,
        });

        let mut request = self
            .agent
            .post(&url)
            .header("content-type", "application/json");
        if let Some(api_key) = self.config.api_key.as_deref() {
            request = request.header("authorization", &format!("Bearer {api_key}"));
        }
        let response = request.send_json(body).map_err(map_ureq_error)?;
        let parsed: ChatResponse = response
            .into_body()
            .with_config()
            .limit(MAX_RESPONSE_BYTES)
            .read_json()
            .map_err(|_| L1Error::InvalidResponse)?;

        let content = parsed
            .choices
            .into_iter()
            .next()
            .and_then(|choice| choice.message.content)
            .filter(|c| !c.trim().is_empty())
            .ok_or(L1Error::InvalidResponse)?;

        Ok((content, dlp_report))
    }
}

fn is_local_endpoint(endpoint: &str) -> bool {
    let Some((scheme, rest)) = endpoint.split_once("://") else {
        return false;
    };
    if scheme != "http" || rest.contains('@') || rest.contains('?') || rest.contains('#') {
        return false;
    }
    let Some((authority, path)) = rest.split_once('/') else {
        return false;
    };
    if path.trim_end_matches('/') != "v1" {
        return false;
    }
    let host = authority
        .rsplit_once(':')
        .map_or(authority, |(host, port)| {
            if port.is_empty() || port.parse::<u16>().is_err() {
                ""
            } else {
                host
            }
        });
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

fn map_ureq_error(error: ureq::Error) -> L1Error {
    match error {
        ureq::Error::StatusCode(code) => L1Error::HttpStatus(code),
        ureq::Error::Timeout(_) => L1Error::Timeout,
        ureq::Error::Io(error) if error.kind() == std::io::ErrorKind::TimedOut => L1Error::Timeout,
        ureq::Error::BadUri(_) => L1Error::InvalidEndpoint,
        ureq::Error::HostNotFound | ureq::Error::ConnectionFailed => L1Error::Offline,
        _ => L1Error::Offline,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn local_server(response: &'static str) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local test server");
        let address = format!("http://{}/v1", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept client");
            let mut request = [0_u8; 4096];
            let size = stream.read(&mut request).expect("read request");
            let request = String::from_utf8_lossy(&request[..size]);
            assert!(request.starts_with("POST /v1/chat/completions"));
            let payload = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response.len(),
                response
            );
            stream
                .write_all(payload.as_bytes())
                .expect("write response");
        });
        (address, handle)
    }

    #[test]
    fn sends_openai_request_and_returns_text() {
        let (endpoint, server) = local_server(
            r#"{"choices":[{"message":{"role":"assistant","content":"hello locally"}}]}"#,
        );
        let client = L1Client::new(L1Config {
            endpoint,
            model: "test-model".into(),
            max_tokens: 42,
            temperature: 0.3,
            timeout: Duration::from_secs(2),
            api_key: Some("must-not-be-logged".into()),
        })
        .unwrap();
        let (response, _) = client.complete("ping").unwrap();
        assert_eq!(response, "hello locally");
        server.join().unwrap();
    }

    #[test]
    fn offline_service_is_typed_without_secret() {
        let client = L1Client::new(L1Config {
            endpoint: "http://127.0.0.1:1/v1".into(),
            api_key: Some("secret-token".into()),
            timeout: Duration::from_millis(100),
            ..L1Config::default()
        })
        .unwrap();
        let error = client.complete("ping").unwrap_err();
        assert!(matches!(error, L1Error::Offline | L1Error::Timeout));
        assert!(!error.to_string().contains("secret-token"));
    }

    #[test]
    fn empty_prompt_is_rejected_before_network() {
        let client = L1Client::new(L1Config::default()).unwrap();
        assert!(matches!(client.complete("  "), Err(L1Error::EmptyPrompt)));
    }

    #[test]
    fn rejects_non_local_endpoints() {
        for endpoint in [
            "https://127.0.0.1:1234/v1",
            "http://10.0.0.2:1234/v1",
            "http://127.0.0.1:1234/v1?x=1",
        ] {
            assert!(matches!(
                L1Client::new(L1Config {
                    endpoint: endpoint.into(),
                    ..Default::default()
                }),
                Err(L1Error::InvalidEndpoint)
            ));
        }
    }

    #[test]
    fn rejects_oversized_prompt() {
        let client = L1Client::new(L1Config::default()).unwrap();
        assert!(matches!(
            client.complete(&"x".repeat(MAX_PROMPT_BYTES + 1)),
            Err(L1Error::InvalidResponse)
        ));
    }

    #[test]
    fn sanitizes_prompt_with_dlp_filter() {
        let (endpoint, server) = local_server(
            r#"{"choices":[{"message":{"role":"assistant","content":"sanitized response"}}]}"#,
        );
        let client = L1Client::new(L1Config {
            endpoint,
            model: "test-model".into(),
            max_tokens: 512,
            temperature: 0.2,
            timeout: Duration::from_secs(2),
            api_key: None,
        })
        .unwrap();
        let (response, dlp_report) = client
            .complete("use ghp_1234567890123456789012345678901 --password=hunter2")
            .unwrap();
        assert_eq!(response, "sanitized response");
        assert_eq!(dlp_report.redaction_count, 2);
        assert!(dlp_report
            .rules_applied
            .contains(&"GitHub Token".to_string()));
        assert!(dlp_report
            .rules_applied
            .contains(&"Command Password".to_string()));
        server.join().unwrap();
    }
}

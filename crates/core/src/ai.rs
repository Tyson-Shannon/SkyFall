//! Model-management proxy for the LLM narrator (phase 6).
//!
//! The narrator talks to an OpenAI-compatible endpoint, but on a fresh machine the
//! model itself usually is not present yet: Ollama is installed and running, with
//! zero models, so every narration call 404s. Typing a model name into Settings
//! cannot fix that, so these helpers let the app find out what is missing and fetch
//! it with a progress bar.
//!
//! Nothing here is Ollama-specific in a hard way. We probe `/api/tags`; if that
//! 404s we assume a remote/cloud endpoint whose models we cannot enumerate, and
//! report it as externally managed instead of nagging the user to install
//! something they do not control.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Keep probes short: this runs while a settings dialog is open.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Must match the Python sidecar's defaults so the UI can show the effective
/// target even before a sidecar config file exists.
pub const DEFAULT_BASE_URL: &str = "http://localhost:11434";
/// 3B is the sweet spot on consumer/CPU-only machines; see the Settings copy.
pub const DEFAULT_MODEL: &str = "qwen2.5:3b";

/// What the settings dialog needs to render an honest status line + install button.
#[derive(Debug, Clone, Serialize)]
pub struct AiStatus {
    /// The endpoint answered at all.
    pub reachable: bool,
    /// Endpoint speaks the Ollama API, so we can list and pull models.
    pub is_ollama: bool,
    /// The configured model is already downloaded.
    pub model_installed: bool,
    /// Model we checked (as configured).
    pub model: String,
    /// Endpoint the narrator will call.
    pub base_url: String,
    /// Models already present locally (empty for cloud endpoints).
    pub installed_models: Vec<String>,
    /// Rough download size for the configured model, when we know it.
    pub size_hint: Option<String>,
    /// Human-readable explanation, shown verbatim in the UI.
    pub detail: String,
}

/// Known on-disk sizes for the models we suggest. Deliberately coarse: it is a
/// "this will take a while" signal, not a precise figure.
fn size_hint(model: &str) -> Option<&'static str> {
    match model {
        "qwen2.5:0.5b" => Some("~400 MB"),
        "qwen2.5:1.5b" => Some("~1.0 GB"),
        "qwen2.5:3b" => Some("~1.9 GB"),
        "qwen2.5:7b" => Some("~4.7 GB"),
        "qwen2.5:14b" => Some("~9 GB"),
        _ => None,
    }
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .build()
        .unwrap_or_default()
}

/// Ollama exposes its API at the root; the narrator uses the OpenAI-compatible
/// `/v1` path underneath it, so strip any `/v1` before calling `/api/tags`.
fn ollama_root(base_url: &str) -> String {
    let b = base_url.trim_end_matches('/');
    b.strip_suffix("/v1").unwrap_or(b).to_string()
}

fn collect_models(tags: &Value) -> Vec<String> {
    tags.get("models")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("name").and_then(|n| n.as_str()))
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Ollama tags models inconsistently with what users type: it reports
/// `qwen2.5:3b` but also ships `qwen2.5:3b-instruct`, and users often type the
/// bare family (`qwen2.5`) to mean "whatever tag is newest". Compare normalized
/// forms so we never nag about a model that is in fact present.
fn normalize(model: &str) -> &str {
    model.strip_suffix("-instruct").unwrap_or(model)
}

fn family(model: &str) -> &str {
    model.split(':').next().unwrap_or(model)
}

fn model_present(installed: &[String], want: &str) -> bool {
    let w = normalize(want.trim());
    installed.iter().any(|m| {
        let n = normalize(m);
        n == w || (!w.contains(':') && family(n) == w)
    })
}

/// Probe the endpoint and describe the model situation.
pub async fn status(base_url: &str, model: &str) -> AiStatus {
    let root = ollama_root(base_url);
    let mut st = AiStatus {
        reachable: false,
        is_ollama: false,
        model_installed: false,
        model: model.to_string(),
        base_url: base_url.to_string(),
        installed_models: Vec::new(),
        size_hint: size_hint(model).map(str::to_string),
        detail: String::new(),
    };

    let url = format!("{root}/api/tags");
    let resp = match http().get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            st.detail = format!("Cannot reach the AI endpoint at {root} ({e}).");
            return st;
        }
    };

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        // Answers, but no Ollama API: a cloud provider. We cannot install those.
        st.reachable = true;
        st.is_ollama = false;
        st.model_installed = true;
        st.detail = "Using a remote AI provider; models are managed there.".into();
        return st;
    }
    if !resp.status().is_success() {
        st.detail = format!("AI endpoint returned HTTP {}.", resp.status());
        return st;
    }

    st.reachable = true;
    st.is_ollama = true;
    let tags: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            st.detail = format!("Could not read model list ({e}).");
            return st;
        }
    };
    st.installed_models = collect_models(&tags);
    st.model_installed = model_present(&st.installed_models, model);
    st.detail = if st.model_installed {
        format!("{} is installed.", model)
    } else if st.installed_models.is_empty() {
        format!("No models installed yet. {model} needs downloading.")
    } else {
        format!("{model} is not installed.")
    };
    st
}

/// One line of progress from Ollama's `/api/pull` NDJSON stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullLine {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl PullLine {
    /// A local failure (endpoint unreachable, HTTP error) in stream shape, so the
    /// UI reads progress and errors through one code path.
    pub fn failed(message: impl Into<String>) -> PullLine {
        PullLine {
            status: String::new(),
            digest: None,
            total: None,
            completed: None,
            error: Some(message.into()),
        }
    }

    /// The download was stopped by the user. Not an error: it can be resumed.
    pub fn cancelled() -> PullLine {
        PullLine {
            status: "cancelled".into(),
            digest: None,
            total: None,
            completed: None,
            error: None,
        }
    }
}

/// Stream a model download, yielding one [`PullLine`] per upstream NDJSON line.
///
/// A pull is long-lived, so the short probe timeout must not apply here.
pub async fn pull(
    base_url: &str,
    model: &str,
) -> Result<impl futures_util::Stream<Item = Result<PullLine, String>>> {
    let root = ollama_root(base_url);
    let resp = http()
        .post(format!("{root}/api/pull"))
        .timeout(Duration::from_secs(60 * 60))
        .json(&serde_json::json!({ "model": model, "stream": true }))
        .send()
        .await
        .with_context(|| format!("cannot reach the AI endpoint at {root}"))?;

    if !resp.status().is_success() {
        let code = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("AI endpoint returned HTTP {code}: {body}"));
    }

    let upstream = resp.bytes_stream().map(|chunk| match chunk {
        Ok(b) => Ok(b),
        Err(e) => Err(e.to_string()),
    });

    // NDJSON chunks split at arbitrary byte boundaries, so carry a remainder
    // across chunks and only emit complete lines.
    fn parse_line(text: &str) -> PullLine {
        let text = text.trim();
        serde_json::from_str(text).unwrap_or_else(|_| PullLine {
            status: text.to_string(),
            digest: None,
            total: None,
            completed: None,
            error: None,
        })
    }

    let carry: Vec<u8> = Vec::new();
    let lines = futures_util::stream::unfold(
        (Box::pin(upstream), carry),
        move |(mut stream, mut carry)| async move {
            loop {
                // Drain complete lines already buffered *before* polling: the
                // whole body often arrives in one chunk, and treating the final
                // empty read as "flush everything" would merge the tail lines.
                if let Some(nl) = carry.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = carry.drain(..=nl).collect();
                    if line.iter().all(|b| b.is_ascii_whitespace()) {
                        continue; // keep-alive blank line
                    }
                    let text = String::from_utf8_lossy(&line).into_owned();
                    return Some((Ok(parse_line(&text)), (stream, carry)));
                }
                match stream.next().await {
                    Some(Ok(b)) => carry.extend_from_slice(&b),
                    Some(Err(e)) => return Some((Err(e), (stream, carry))),
                    // Upstream finished: flush any trailing partial line.
                    None => {
                        if carry.iter().all(|b| b.is_ascii_whitespace()) {
                            return None;
                        }
                        let rest = String::from_utf8_lossy(&carry).trim().to_string();
                        return Some((Ok(parse_line(&rest)), (stream, Vec::new())));
                    }
                }
            }
        },
    );

    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway HTTP endpoint, so these tests never touch a real Ollama (a
    /// stray pull would download gigabytes). Deliberately raw TCP: the app ships
    /// no web server, and tests should not drag one back in as a dependency.
    ///
    /// `routes` maps a request line's path to a canned `body`; the first path in
    /// the table is the fallback, which is how the 404 probe is answered.
    struct Mock {
        url: String,
        _dir: std::path::PathBuf,
    }

    /// Bind on an ephemeral port and return a URL. The listener stays open until
    /// the returned `Mock` drops, which unblocks the accept loop.
    fn mock(routes: Vec<(&'static str, &'static str, u16)>) -> Mock {
        let dir = std::env::temp_dir().join(format!(
            "skyfall-ai-mock-{}-{}",
            std::process::id(),
            routes.len()
        ));
        std::fs::create_dir_all(&dir).expect("mock dir");
        let marker = dir.join("up");
        let listener = std::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
            .expect("bind mock");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let ready = marker.clone();

        std::thread::spawn(move || {
            std::fs::write(&marker, b"1").ok();
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { continue };
                use std::io::{Read, Write};
                let mut buf = [0u8; 2048];
                let n = conn.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let (body, code) = routes
                    .iter()
                    .find(|(p, _, _)| req.contains(p))
                    .map(|(_, b, c)| (*b, *c))
                    .unwrap_or(("not found", 404));
                let head = format!(
                    "HTTP/1.1 {code} X\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = conn.write_all(head.as_bytes());
                let _ = conn.write_all(body.as_bytes());
                let _ = conn.flush();
            }
        });

        // Wait until the accept loop is actually running.
        for _ in 0..200 {
            if ready.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        Mock { url, _dir: dir }
    }

    fn tags_url(models: &'static str) -> String {
        mock(vec![("/api/tags", models, 200)]).url
    }

    #[tokio::test]
    async fn status_reports_installed_model() {
        let url = tags_url(r#"{"models":[{"name":"qwen2.5:3b"},{"name":"llama3.2:1b"}]}"#);
        let s = status(&url, "qwen2.5:3b").await;
        assert!(s.reachable && s.is_ollama && s.model_installed);
        assert_eq!(s.installed_models.len(), 2);
        assert_eq!(s.size_hint.as_deref(), Some("~1.9 GB"));
    }

    #[tokio::test]
    async fn status_reports_missing_model() {
        let url = tags_url(r#"{"models":[{"name":"llama3.2:1b"}]}"#);
        let s = status(&url, "qwen2.5:3b").await;
        assert!(s.reachable && s.is_ollama);
        assert!(!s.model_installed, "3b is not in the list");
        assert!(s.detail.contains("not installed"));
    }

    #[tokio::test]
    async fn status_handles_no_models_at_all() {
        // The real state of a fresh Ollama install.
        let url = tags_url(r#"{"models":[]}"#);
        let s = status(&url, DEFAULT_MODEL).await;
        assert!(s.reachable && s.is_ollama && !s.model_installed);
        assert!(s.detail.contains("No models installed"));
    }

    #[tokio::test]
    async fn status_treats_instruct_suffix_as_present() {
        let url = tags_url(r#"{"models":[{"name":"qwen2.5:3b-instruct"}]}"#);
        assert!(status(&url, "qwen2.5:3b").await.model_installed);
    }

    #[tokio::test]
    async fn status_treats_404_as_remote_provider() {
        // A cloud endpoint answers, but has no /api/tags: nothing for us to install.
        let url = mock(vec![("/api/nope", r#"{"ok":true}"#, 200)]).url;
        let s = status(&url, "gpt-4o-mini").await;
        assert!(s.reachable, "endpoint answered");
        assert!(!s.is_ollama, "not an Ollama API");
        assert!(
            s.model_installed,
            "remote models are managed by the provider"
        );
        assert!(s.detail.contains("remote"));
    }

    #[tokio::test]
    async fn status_reports_unreachable_endpoint() {
        // Port 1 is reserved and never listening.
        let s = status("http://127.0.0.1:1", DEFAULT_MODEL).await;
        assert!(!s.reachable);
        assert!(s.detail.contains("Cannot reach"));
    }

    #[tokio::test]
    async fn ollama_root_strips_openai_suffix() {
        assert_eq!(ollama_root("http://h:11434/"), "http://h:11434");
        assert_eq!(ollama_root("http://h:11434/v1"), "http://h:11434");
        assert_eq!(ollama_root("http://h:11434/v1/"), "http://h:11434");
    }

    #[tokio::test]
    async fn pull_streams_progress_lines() {
        use futures_util::StreamExt as _;

        let body = concat!(
            r#"{"status":"pulling manifest"}"#,
            "\n",
            r#"{"status":"pulling sha256:aa","total":1000,"completed":250}"#,
            "\n",
            r#"{"status":"pulling sha256:aa","total":1000,"completed":1000}"#,
            "\n",
            r#"{"status":"success"}"#,
            "\n"
        );
        let url = mock(vec![("/api/pull", body, 200)]).url;

        let lines: Vec<_> = pull(&url, "qwen2.5:3b")
            .await
            .expect("pull starts")
            .map(|r| r.expect("line"))
            .collect()
            .await;

        assert_eq!(lines.len(), 4, "one per NDJSON line: {lines:?}");
        assert_eq!(lines[0].status, "pulling manifest");
        assert_eq!(lines[1].completed, Some(250));
        assert_eq!(lines[2].completed, Some(1000));
        assert_eq!(lines[3].status, "success");
    }

    #[tokio::test]
    async fn pull_rejects_lines_split_across_chunks() {
        // reqwest may split NDJSON anywhere; the reader must reassemble.
        let body = r#"{"status":"pulling manifest"}
{"status":"pulling sha256:bb","total":2000,"completed":1999}
{"status":"success"}
"#;
        let url = mock(vec![("/api/pull", body, 200)]).url;
        let lines: Vec<_> = pull(&url, "m")
            .await
            .expect("pull starts")
            .map(|r| r.expect("line"))
            .collect()
            .await;
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1].total, Some(2000));
    }

    #[tokio::test]
    async fn pull_surfaces_http_errors() {
        let url = mock(vec![("/api/pull", r#"{"error":"bad model"}"#, 400)]).url;
        let err = match pull(&url, "nope").await {
            Ok(_) => panic!("expected an error"),
            Err(e) => format!("{e:#}"),
        };
        assert!(err.contains("400"), "{err}");
    }
}

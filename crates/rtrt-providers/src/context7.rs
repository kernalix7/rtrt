//! context7 library-docs fetcher.
//!
//! Wraps the public context7 API at `https://context7.com/api/v1/<library>`,
//! which returns version-pinned, LLM-friendly documentation for a library
//! given its `<owner>/<repo>` identifier (e.g. `facebook/react`).
//!
//! The fetcher lives in `rtrt-providers` because it shares the same `reqwest`
//! client and TLS stack as the chat providers; downstream surfaces
//! (`rtrt-mcp`, `rtrt-cli docs`, dashboard) compose it without pulling extra
//! HTTP deps.

use rtrt_core::{Error, Result};

pub struct Context7Client {
    pub base_url: String,
    pub http: reqwest::Client,
}

impl Default for Context7Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Context7Client {
    pub fn new() -> Self {
        Self {
            base_url: "https://context7.com/api/v1".to_string(),
            http: reqwest::Client::new(),
        }
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    pub fn with_http(mut self, http: reqwest::Client) -> Self {
        self.http = http;
        self
    }

    /// Fetches docs for `library_id` (e.g. `"facebook/react"`). When `topic`
    /// is set, context7 narrows the result to the matching docs section.
    pub async fn get_library_docs(&self, library_id: &str, topic: Option<&str>) -> Result<String> {
        if !is_valid_library_id(library_id) {
            return Err(Error::Provider(format!(
                "context7: invalid library id '{library_id}' (expected '<owner>/<repo>')"
            )));
        }
        let url = format!("{}/{}", self.base_url, library_id);
        let mut req = self.http.get(&url).header("accept", "text/plain");
        if let Some(t) = topic {
            req = req.query(&[("topic", t)]);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| Error::Provider(format!("context7: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Provider(format!("context7 {status}: {body}")));
        }
        resp.text()
            .await
            .map_err(|e| Error::Provider(format!("context7 body: {e}")))
    }
}

fn is_valid_library_id(s: &str) -> bool {
    // <owner>/<repo>, each piece [a-zA-Z0-9._-]+
    let mut parts = s.split('/');
    let owner = parts.next();
    let repo = parts.next();
    if parts.next().is_some() {
        return false;
    }
    match (owner, repo) {
        (Some(o), Some(r)) => is_segment(o) && is_segment(r),
        _ => false,
    }
}

fn is_segment(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn get_docs_round_trip() {
        if loopback_bind_unavailable() {
            return;
        }
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("GET", "/facebook/react")
            .with_status(200)
            .with_body("# React docs\n…")
            .create_async()
            .await;
        let client = Context7Client::new().with_base_url(server.url());
        let out = client
            .get_library_docs("facebook/react", None)
            .await
            .unwrap();
        assert!(out.starts_with("# React docs"));
    }

    #[tokio::test]
    async fn topic_query_parameter_is_sent() {
        if loopback_bind_unavailable() {
            return;
        }
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("GET", "/facebook/react")
            .match_query(mockito::Matcher::UrlEncoded("topic".into(), "hooks".into()))
            .with_status(200)
            .with_body("hooks docs")
            .create_async()
            .await;
        let client = Context7Client::new().with_base_url(server.url());
        let out = client
            .get_library_docs("facebook/react", Some("hooks"))
            .await
            .unwrap();
        assert_eq!(out, "hooks docs");
    }

    #[test]
    fn rejects_invalid_library_ids() {
        let c = Context7Client::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            assert!(c.get_library_docs("bad", None).await.is_err());
            assert!(c.get_library_docs("owner//repo", None).await.is_err());
            assert!(c.get_library_docs("owner/repo/extra", None).await.is_err());
            assert!(c.get_library_docs("../escape/repo", None).await.is_err());
        });
    }

    fn loopback_bind_unavailable() -> bool {
        std::net::TcpListener::bind("127.0.0.1:0").is_err()
    }
}

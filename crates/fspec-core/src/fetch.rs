use std::collections::HashMap;
use std::io::Read;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};

/// HTTP reads for resolvers and the store. Tests use [`MapFetch`].
pub trait Fetch: Sync {
    fn get(&self, url: &str) -> Result<Vec<u8>>;

    /// A GET with extra request headers that also returns the response headers (registry
    /// manifests carry their digest there). Implementations without headers fall back to
    /// [`Fetch::get`].
    fn request(&self, url: &str, _headers: &[(&str, &str)]) -> Result<Response> {
        Ok(Response { headers: vec![], body: self.get(url)? })
    }

    fn get_json(&self, url: &str) -> Result<serde_json::Value> {
        serde_json::from_slice(&self.get(url)?).with_context(|| format!("parsing JSON from {url}"))
    }
}

/// A response body with its headers, names lowercased.
#[derive(Clone, Debug, Default)]
pub struct Response {
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// HTTP(S) through ureq; `file://` URLs read the local file, for fixtures.
pub struct LiveFetch {
    agent: ureq::Agent,
    /// Sent as a bearer token to api.github.com when set (`GITHUB_TOKEN`).
    github_token: Option<String>,
}

impl LiveFetch {
    pub fn new() -> LiveFetch {
        LiveFetch { agent: ureq::Agent::new_with_defaults(), github_token: std::env::var("GITHUB_TOKEN").ok() }
    }
}

impl Default for LiveFetch {
    fn default() -> Self {
        LiveFetch::new()
    }
}

impl Fetch for LiveFetch {
    fn get(&self, url: &str) -> Result<Vec<u8>> {
        Ok(self.request(url, &[])?.body)
    }

    fn request(&self, url: &str, headers: &[(&str, &str)]) -> Result<Response> {
        if let Some(path) = url.strip_prefix("file://") {
            let body = std::fs::read(path).with_context(|| format!("reading {url}"))?;
            return Ok(Response { headers: vec![], body });
        }
        let mut req = self.agent.get(url).header("User-Agent", "fspec");
        if let (Some(token), true) = (&self.github_token, url.starts_with("https://api.github.com/")) {
            req = req.header("Authorization", &format!("Bearer {token}"));
        }
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = req.call().with_context(|| format!("GET {url}"))?;
        let headers = resp
            .headers()
            .iter()
            .filter_map(|(k, v)| Some((k.as_str().to_ascii_lowercase(), v.to_str().ok()?.to_string())))
            .collect();
        let mut body = Vec::new();
        resp.into_body().into_reader().read_to_end(&mut body).with_context(|| format!("reading {url}"))?;
        Ok(Response { headers, body })
    }
}

/// A recorded request: the URL and the headers it carried.
pub type SentRequest = (String, Vec<(String, String)>);

/// Serves fixed bodies by exact URL.
#[derive(Default)]
pub struct MapFetch {
    pub bodies: HashMap<String, Vec<u8>>,
    /// Response headers per URL, for [`Fetch::request`].
    pub response_headers: HashMap<String, Vec<(String, String)>>,
    /// Every [`Fetch::request`] made.
    pub requests: Mutex<Vec<SentRequest>>,
}

impl MapFetch {
    pub fn with(mut self, url: &str, body: impl Into<Vec<u8>>) -> MapFetch {
        self.bodies.insert(url.into(), body.into());
        self
    }

    pub fn with_headers(mut self, url: &str, headers: &[(&str, &str)]) -> MapFetch {
        self.response_headers.insert(url.into(), headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect());
        self
    }
}

impl Fetch for MapFetch {
    fn get(&self, url: &str) -> Result<Vec<u8>> {
        self.bodies.get(url).cloned().ok_or_else(|| anyhow!("MapFetch has no body for {url}"))
    }

    fn request(&self, url: &str, headers: &[(&str, &str)]) -> Result<Response> {
        let sent = headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        self.requests.lock().unwrap().push((url.to_string(), sent));
        Ok(Response { headers: self.response_headers.get(url).cloned().unwrap_or_default(), body: self.get(url)? })
    }
}

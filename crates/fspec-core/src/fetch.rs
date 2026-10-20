use std::collections::HashMap;
use std::io::Read;

use anyhow::{Context, Result, anyhow};

/// HTTP reads for resolvers and the store. Tests use [`MapFetch`].
pub trait Fetch: Sync {
    fn get(&self, url: &str) -> Result<Vec<u8>>;

    fn get_json(&self, url: &str) -> Result<serde_json::Value> {
        serde_json::from_slice(&self.get(url)?).with_context(|| format!("parsing JSON from {url}"))
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
        if let Some(path) = url.strip_prefix("file://") {
            return std::fs::read(path).with_context(|| format!("reading {url}"));
        }
        let mut req = self.agent.get(url).header("User-Agent", "fspec");
        if let (Some(token), true) = (&self.github_token, url.starts_with("https://api.github.com/")) {
            req = req.header("Authorization", &format!("Bearer {token}"));
        }
        let resp = req.call().with_context(|| format!("GET {url}"))?;
        let mut body = Vec::new();
        resp.into_body().into_reader().read_to_end(&mut body).with_context(|| format!("reading {url}"))?;
        Ok(body)
    }
}

/// Serves fixed bodies by exact URL.
#[derive(Default)]
pub struct MapFetch {
    pub bodies: HashMap<String, Vec<u8>>,
}

impl MapFetch {
    pub fn with(mut self, url: &str, body: impl Into<Vec<u8>>) -> MapFetch {
        self.bodies.insert(url.into(), body.into());
        self
    }
}

impl Fetch for MapFetch {
    fn get(&self, url: &str) -> Result<Vec<u8>> {
        self.bodies.get(url).cloned().ok_or_else(|| anyhow!("MapFetch has no body for {url}"))
    }
}

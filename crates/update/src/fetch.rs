use crate::{Error, Result};
use std::io::{Read, Write};
use std::time::Duration;

/// Streams a URL into a writer, failing if the body exceeds `limit` bytes.
pub trait Fetch {
    fn get(&self, url: &str, limit: u64, sink: &mut dyn Write) -> Result<()>;
}

/// HTTPS through `ureq` (plain HTTP only when `allow_http` is set, for local test servers).
pub struct Http {
    agent: ureq::Agent,
    allow_http: bool,
}

impl Http {
    pub fn new() -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(30))
            .redirects(5)
            .user_agent(concat!("codecraft-update/", env!("CARGO_PKG_VERSION")))
            .build();
        Self { agent, allow_http: false }
    }
    /// Allow `http://` (loopback servers in tests).
    pub fn allow_http(mut self) -> Self {
        self.allow_http = true;
        self
    }
}

impl Default for Http {
    fn default() -> Self {
        Self::new()
    }
}

impl Fetch for Http {
    fn get(&self, url: &str, limit: u64, sink: &mut dyn Write) -> Result<()> {
        if !(url.starts_with("https://") || (self.allow_http && url.starts_with("http://"))) {
            return Err(Error::Network("only https URLs are fetched".into()));
        }
        let response = self.agent.get(url).call().map_err(|e| Error::Network(e.to_string()))?;
        if let Some(len) = response.header("content-length").and_then(|v| v.parse::<u64>().ok())
            && len > limit
        {
            return Err(Error::Network(format!("{url} is {len} bytes, more than the {limit} allowed")));
        }
        let mut body = response.into_reader().take(limit.saturating_add(1));
        let mut buf = [0u8; 64 * 1024];
        let mut total: u64 = 0;
        loop {
            let n = body.read(&mut buf).map_err(|e| Error::Network(e.to_string()))?;
            if n == 0 {
                break;
            }
            total = total.saturating_add(n as u64);
            if total > limit {
                return Err(Error::Network(format!("{url} is larger than the {limit} bytes allowed")));
            }
            sink.write_all(buf.get(..n).unwrap_or_default())?;
        }
        Ok(())
    }
}

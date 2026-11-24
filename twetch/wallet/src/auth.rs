//! Who may talk to the sidecar.
//!
//! Two bearer tokens: the proxy token (a 0600 file, handed to the coding agent)
//! can only run requests and read the balance; the admin token is never on
//! disk and only the desktop app that started the sidecar holds it. The wallet
//! page is reached through a one-time link that only the admin token can mint,
//! and needs both a cookie and a secret kept by that page's own origin:
//! cookies are shared by every port of 127.0.0.1, the page storage is not.

use axum::http::{header, HeaderMap};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const LINK_TTL: Duration = Duration::from_secs(120);
pub const COOKIE: &str = "twetch_wallet";
pub const SESSION_HEADER: &str = "x-twetch-session";

fn digest(s: &str) -> [u8; 32] {
    Sha256::digest(s.as_bytes()).into()
}

/// # Errors
/// When the OS has no entropy to give.
pub fn random_token() -> anyhow::Result<String> {
    let mut b = [0u8; 32];
    getrandom::getrandom(&mut b).map_err(|e| anyhow::anyhow!("no entropy: {e}"))?;
    Ok(hex::encode(b))
}

/// # Errors
/// When the directory cannot be made private.
pub fn private_dir(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// # Errors
/// When the token file cannot be read or written.
pub fn load_or_create_token(path: &Path) -> anyhow::Result<String> {
    if let Ok(t) = std::fs::read_to_string(path) {
        let t = t.trim();
        if t.len() == 64 {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            }
            return Ok(t.to_string());
        }
    }
    let token = random_token()?;
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    f.write_all(token.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(token)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Who {
    Proxy,
    Admin,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    Host,
    Browser,
    Token,
}

struct Session {
    page: [u8; 32],
}

pub struct Auth {
    hosts: [String; 2],
    origins: [String; 2],
    proxy: [u8; 32],
    admin: [u8; 32],
    links: Mutex<HashMap<[u8; 32], Instant>>,
    sessions: Mutex<HashMap<[u8; 32], Session>>,
}

impl Auth {
    #[must_use]
    pub fn new(port: u16, proxy: &str, admin: &str) -> Self {
        Self {
            hosts: [format!("127.0.0.1:{port}"), format!("localhost:{port}")],
            origins: [format!("http://127.0.0.1:{port}"), format!("http://localhost:{port}")],
            proxy: digest(proxy),
            admin: digest(admin),
            links: Mutex::default(),
            sessions: Mutex::default(),
        }
    }

    /// A rebound name sends its own Host: only ours is served.
    ///
    /// # Errors
    /// When the request was made under another name.
    pub fn host(&self, h: &HeaderMap) -> Result<(), Refusal> {
        let host = h.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
        if self.hosts.iter().any(|x| x == host) {
            Ok(())
        } else {
            Err(Refusal::Host)
        }
    }

    /// A program's request: a bearer token, and no sign of a browser page.
    ///
    /// # Errors
    /// When the host, the caller or the token is wrong.
    pub fn bearer(&self, h: &HeaderMap) -> Result<Who, Refusal> {
        self.host(h)?;
        if h.contains_key(header::ORIGIN) || h.contains_key("sec-fetch-site") {
            return Err(Refusal::Browser);
        }
        let token = h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).ok_or(Refusal::Token)?;
        // Compared as digests, so timing says nothing about the token.
        let d = digest(token.trim());
        if d == self.admin {
            Ok(Who::Admin)
        } else if d == self.proxy {
            Ok(Who::Proxy)
        } else {
            Err(Refusal::Token)
        }
    }

    /// A one-time wallet link token.
    ///
    /// # Errors
    /// When the OS has no entropy to give.
    pub fn mint_link(&self) -> anyhow::Result<String> {
        let t = random_token()?;
        let mut links = self.links.lock().expect("links");
        links.retain(|_, at| at.elapsed() < LINK_TTL);
        links.insert(digest(&t), Instant::now());
        drop(links);
        Ok(t)
    }

    /// Trade a link for a session: `(cookie, page secret)`. Works once.
    ///
    /// # Errors
    /// When the link is unknown, used or old, or there is no entropy.
    pub fn redeem(&self, link: &str) -> anyhow::Result<(String, String)> {
        let at = self.links.lock().expect("links").remove(&digest(link));
        anyhow::ensure!(at.is_some_and(|at| at.elapsed() < LINK_TTL), "this wallet link was used or is too old");
        let (cookie, page) = (random_token()?, random_token()?);
        self.sessions.lock().expect("sessions").insert(digest(&cookie), Session { page: digest(&page) });
        Ok((cookie, page))
    }

    fn cookie(h: &HeaderMap) -> Option<&str> {
        h.get(header::COOKIE)?.to_str().ok()?.split(';').filter_map(|c| c.trim().split_once('=')).find_map(|(k, v)| (k == COOKIE).then_some(v))
    }

    /// The wallet page itself: a cookie is enough to see the shell, which holds no data.
    #[must_use]
    pub fn has_cookie(&self, h: &HeaderMap) -> bool {
        Self::cookie(h).is_some_and(|c| self.sessions.lock().expect("sessions").contains_key(&digest(c)))
    }

    /// A call from the wallet page.
    ///
    /// # Errors
    /// When the host, the origin, the cookie or the page secret is wrong.
    pub fn page(&self, h: &HeaderMap, writes: bool) -> Result<(), Refusal> {
        self.host(h)?;
        let origin = h.get(header::ORIGIN).and_then(|v| v.to_str().ok());
        match origin {
            Some(o) if self.origins.iter().any(|x| x == o) => {}
            None if !writes => {}
            _ => return Err(Refusal::Browser),
        }
        if h.get("sec-fetch-site").is_some_and(|v| v != "same-origin") {
            return Err(Refusal::Browser);
        }
        let cookie = Self::cookie(h).ok_or(Refusal::Token)?;
        let page = h.get(SESSION_HEADER).and_then(|v| v.to_str().ok()).ok_or(Refusal::Token)?;
        let sessions = self.sessions.lock().expect("sessions");
        match sessions.get(&digest(cookie)) {
            Some(s) if s.page == digest(page) => Ok(()),
            _ => Err(Refusal::Token),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().unwrap());
        }
        h
    }

    const HOST: (&str, &str) = ("host", "127.0.0.1:9");

    type Case<'a> = (&'a [(&'static str, &'a str)], Result<Who, Refusal>);

    #[test]
    fn bearer_rules() {
        let a = Auth::new(9, "proxy", "admin");
        let cases: &[Case] = &[
            (&[HOST, ("authorization", "Bearer proxy")], Ok(Who::Proxy)),
            (&[("host", "localhost:9"), ("authorization", "Bearer admin")], Ok(Who::Admin)),
            (&[HOST, ("authorization", "Bearer nope")], Err(Refusal::Token)),
            (&[HOST, ("authorization", "Bearer ")], Err(Refusal::Token)),
            (&[HOST], Err(Refusal::Token)),
            // DNS rebinding: the page's own name arrives as Host.
            (&[("host", "evil.example:9"), ("authorization", "Bearer proxy")], Err(Refusal::Host)),
            (&[("host", "127.0.0.1:10"), ("authorization", "Bearer proxy")], Err(Refusal::Host)),
            (&[("authorization", "Bearer proxy")], Err(Refusal::Host)),
            // Any page, even one that learned the token.
            (&[HOST, ("origin", "http://evil.example"), ("authorization", "Bearer proxy")], Err(Refusal::Browser)),
            (&[HOST, ("origin", "null"), ("authorization", "Bearer proxy")], Err(Refusal::Browser)),
            (&[HOST, ("sec-fetch-site", "cross-site"), ("authorization", "Bearer proxy")], Err(Refusal::Browser)),
        ];
        for (h, want) in cases {
            assert_eq!(a.bearer(&headers(h)), *want, "{h:?}");
        }
    }

    #[test]
    fn links_work_once_and_sessions_need_both_halves() {
        let a = Auth::new(9, "proxy", "admin");
        let link = a.mint_link().unwrap();
        let (cookie, page) = a.redeem(&link).unwrap();
        assert!(a.redeem(&link).is_err(), "used");
        assert!(a.redeem("made up").is_err());
        let c = format!("other=1; {COOKIE}={cookie}");
        let own = ("origin", "http://127.0.0.1:9");
        assert_eq!(a.page(&headers(&[HOST, own, ("cookie", &c), (SESSION_HEADER, &page)]), true), Ok(()));
        assert_eq!(a.page(&headers(&[HOST, ("cookie", &c), (SESSION_HEADER, &page)]), false), Ok(()));
        // A write needs our own origin.
        assert_eq!(a.page(&headers(&[HOST, ("cookie", &c), (SESSION_HEADER, &page)]), true), Err(Refusal::Browser));
        // Another port of 127.0.0.1 gets the cookie, never the page secret.
        assert_eq!(a.page(&headers(&[HOST, own, ("cookie", &c)]), true), Err(Refusal::Token));
        assert_eq!(a.page(&headers(&[HOST, ("origin", "http://127.0.0.1:3000"), ("cookie", &c), (SESSION_HEADER, &page)]), true), Err(Refusal::Browser));
        assert_eq!(a.page(&headers(&[HOST, own, (SESSION_HEADER, &page)]), true), Err(Refusal::Token));
        // The agent's token opens nothing here.
        assert_eq!(a.page(&headers(&[HOST, own, ("authorization", "Bearer proxy")]), true), Err(Refusal::Token));
    }

    #[cfg(unix)]
    #[test]
    fn token_file_is_private_and_stable() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("w").join("t");
        private_dir(p.parent().unwrap()).unwrap();
        let t = load_or_create_token(&p).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(load_or_create_token(&p).unwrap(), t);
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
    }
}

//! 按账号出站隔离的 ChatGPT 基础设施 Cookie 内存 Jar。

use std::sync::{
    RwLock,
    atomic::{AtomicBool, Ordering},
};

use reqwest::{
    Url,
    cookie::{CookieStore, Jar},
    header::HeaderValue,
};
use secrecy::ExposeSecret;

use crate::credential::{CodexCookiePolicy, RuntimeCodexCookie};

#[derive(Debug, Default)]
pub(super) struct InfrastructureCookieStore {
    jar: RwLock<Jar>,
    legacy_seeded: AtomicBool,
}

impl InfrastructureCookieStore {
    /// 旧凭据可能仍带有基础设施 Cookie；每个出站分桶只迁入一次，避免旧值覆盖新响应。
    pub(super) fn seed_legacy(&self, origin: &Url, cookies: &[RuntimeCodexCookie]) {
        if !is_chatgpt_cookie_url(origin)
            || self
                .legacy_seeded
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return;
        }
        let Ok(policy) = CodexCookiePolicy::official() else {
            return;
        };
        let headers = cookies
            .iter()
            .filter(|cookie| {
                crate::credential::cookie::is_infrastructure_cookie(&cookie.name)
                    && cookie
                        .expires_at
                        .is_none_or(|expires_at| expires_at > chrono::Utc::now())
                    && policy.may_replay(
                        origin,
                        &cookie.name,
                        &cookie.domain,
                        &cookie.path,
                        cookie.host_only,
                        cookie.secure,
                    )
            })
            .filter_map(legacy_set_cookie_header)
            .collect::<Vec<_>>();
        self.store_headers(origin, headers.iter());
    }

    pub(super) fn store_response_headers(&self, origin: &Url, headers: &[String]) {
        let headers = headers
            .iter()
            .filter_map(|header| HeaderValue::from_str(header).ok())
            .collect::<Vec<_>>();
        self.store_headers(origin, headers.iter());
    }

    pub(super) fn cookies(&self, target: &Url) -> Option<HeaderValue> {
        <Self as CookieStore>::cookies(self, target).map(|mut header| {
            header.set_sensitive(true);
            header
        })
    }

    pub(super) fn clear(&self) {
        *self
            .jar
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Jar::default();
    }

    fn store_headers<'a>(&self, origin: &Url, headers: impl Iterator<Item = &'a HeaderValue>) {
        let mut headers = headers;
        <Self as CookieStore>::set_cookies(self, &mut headers, origin);
    }
}

impl CookieStore for InfrastructureCookieStore {
    fn set_cookies(&self, cookie_headers: &mut dyn Iterator<Item = &HeaderValue>, url: &Url) {
        if !is_chatgpt_cookie_url(url) {
            return;
        }
        let mut infrastructure_headers = cookie_headers.filter(|header| {
            header
                .to_str()
                .ok()
                .and_then(set_cookie_name)
                .is_some_and(crate::credential::cookie::is_infrastructure_cookie)
        });
        self.jar
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .set_cookies(&mut infrastructure_headers, url);
    }

    fn cookies(&self, url: &Url) -> Option<HeaderValue> {
        if !is_chatgpt_cookie_url(url) {
            return None;
        }
        self.jar
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cookies(url)
            .and_then(only_infrastructure_cookies)
    }
}

fn legacy_set_cookie_header(cookie: &RuntimeCodexCookie) -> Option<HeaderValue> {
    let value = cookie.value.expose_secret();
    if value.is_empty()
        || value.chars().any(char::is_control)
        || value.contains(';')
        || cookie.path.contains(';')
        || cookie.domain.contains(';')
    {
        return None;
    }
    let mut header = format!("{}={}; Path={}", cookie.name, value, cookie.path);
    if !cookie.host_only {
        header.push_str("; Domain=");
        header.push_str(&cookie.domain);
    }
    if cookie.secure {
        header.push_str("; Secure");
    }
    HeaderValue::from_str(&header).ok()
}

fn set_cookie_name(header: &str) -> Option<&str> {
    let (name, _) = header.split_once('=')?;
    let name = name.trim();
    (!name.is_empty()).then_some(name)
}

fn only_infrastructure_cookies(header: HeaderValue) -> Option<HeaderValue> {
    let header = header.to_str().ok()?;
    let cookies = header
        .split(';')
        .filter_map(|cookie| {
            let cookie = cookie.trim();
            let name = cookie.split_once('=')?.0.trim();
            crate::credential::cookie::is_infrastructure_cookie(name).then_some(cookie)
        })
        .collect::<Vec<_>>()
        .join("; ");
    (!cookies.is_empty())
        .then(|| HeaderValue::from_str(&cookies).ok())
        .flatten()
}

fn is_chatgpt_cookie_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url
            .host_str()
            .is_some_and(crate::credential::cookie::is_allowed_chatgpt_host)
}

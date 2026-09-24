//! ChatGPT workspace 路由发现与安全应用。

use std::{collections::HashMap, time::Duration};

use futures::StreamExt;
use reqwest::StatusCode;
use serde::Deserialize;
use thiserror::Error;
use tokio::{sync::RwLock, time::Instant};
use url::Url;

use super::{
    client::{
        CodexBackendClient, CodexClientError, CodexClientResult, CodexRequestContext,
        OpenAiUpstreamProtocol,
    },
    endpoints::account_endpoint_url,
};

const MAX_ACCOUNTS_CHECK_BODY_BYTES: usize = 64 * 1024;
const WORKSPACE_ROUTING_CACHE_TTL: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct WorkspaceRouting {
    pub(super) base_url: String,
    pub(super) routing_override: Option<String>,
}

#[derive(Clone)]
struct CachedWorkspaceRouting {
    routing: WorkspaceRouting,
    expires_at: Instant,
}

#[derive(Default)]
pub(super) struct WorkspaceRoutingCache {
    entries: RwLock<HashMap<String, CachedWorkspaceRouting>>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub(super) enum WorkspaceRoutingError {
    #[error("accounts/check returned status {0}")]
    DiscoveryStatus(StatusCode),
    #[error("accounts/check response exceeded the size limit")]
    ResponseTooLarge,
    #[error("accounts/check response was not valid JSON")]
    InvalidResponse,
    #[error("accounts/check did not return exactly one matching account")]
    AccountMismatch,
    #[error("workspace backend origin was missing")]
    MissingBackendOrigin,
    #[error("workspace backend must be an HTTPS origin")]
    InvalidBackendOrigin,
    #[error("account routing override was not recognized")]
    InvalidRoutingOverride,
    #[error("configured Codex base URL was invalid")]
    InvalidBaseUrl,
}

impl From<WorkspaceRoutingError> for CodexClientError {
    fn from(_: WorkspaceRoutingError) -> Self {
        Self::WorkspaceRouting
    }
}

#[derive(Deserialize)]
struct AccountsCheckResponse {
    #[serde(default)]
    accounts: Vec<AccountEntry>,
}

#[derive(Deserialize)]
struct AccountEntry {
    id: String,
    workspace_backend_origin: Option<String>,
    account_routing_override: Option<String>,
}

impl CodexBackendClient {
    /// 官方 Codex 在发送 Responses/compaction 前先解析 workspace origin 与区域约束。
    pub(super) async fn resolve_workspace_routing(
        &self,
        context: CodexRequestContext<'_>,
    ) -> CodexClientResult<Self> {
        let Some(account_id) = context.account_id else {
            return Ok(self.clone());
        };
        if self.protocol != OpenAiUpstreamProtocol::Codex
            || self.base_url.trim_end_matches('/') != self.official_base_url.trim_end_matches('/')
        {
            return Ok(self.clone());
        }

        let cache_key = format!("{}\0{account_id}", self.egress_key);
        let now = Instant::now();
        if let Some(cached) = self
            .workspace_routing_cache
            .entries
            .read()
            .await
            .get(&cache_key)
            && cached.expires_at > now
        {
            return Ok(self.with_workspace_routing(cached.routing.clone()));
        }

        let response = self
            .client
            .get(account_endpoint_url(&self.base_url, "accounts/check"))
            .headers(self.account_request_headers(context)?)
            .send()
            .await
            .map_err(CodexClientError::HttpJson)?;
        if !response.status().is_success() {
            return Err(WorkspaceRoutingError::DiscoveryStatus(response.status()).into());
        }
        let body = read_capped_body(response).await?;
        let response: AccountsCheckResponse =
            serde_json::from_slice(&body).map_err(|_| WorkspaceRoutingError::InvalidResponse)?;
        let mut matches = response
            .accounts
            .into_iter()
            .filter(|entry| entry.id == account_id);
        let entry = matches
            .next()
            .filter(|_| matches.next().is_none())
            .ok_or(WorkspaceRoutingError::AccountMismatch)?;
        let routing = resolve_routing(entry, &self.base_url)?;
        self.workspace_routing_cache.entries.write().await.insert(
            cache_key,
            CachedWorkspaceRouting {
                routing: routing.clone(),
                expires_at: now + WORKSPACE_ROUTING_CACHE_TTL,
            },
        );
        Ok(self.with_workspace_routing(routing))
    }

    fn with_workspace_routing(&self, routing: WorkspaceRouting) -> Self {
        let mut client = self.clone();
        client.base_url.clone_from(&routing.base_url);
        client.websocket_origin_key = format!(
            "{}:{}",
            super::client::websocket_origin_key(&client.base_url),
            client.egress_key
        );
        client.workspace_routing = Some(routing);
        client
    }
}

fn resolve_routing(
    entry: AccountEntry,
    effective_base_url: &str,
) -> Result<WorkspaceRouting, WorkspaceRoutingError> {
    let routing_override = match entry.account_routing_override.as_deref() {
        Some("NO_CONSTRAINT") => None,
        Some(value @ ("us" | "us_cr")) => Some(value.to_owned()),
        _ => return Err(WorkspaceRoutingError::InvalidRoutingOverride),
    };
    let backend = entry
        .workspace_backend_origin
        .ok_or(WorkspaceRoutingError::MissingBackendOrigin)?;
    let mut base =
        Url::parse(effective_base_url).map_err(|_| WorkspaceRoutingError::InvalidBaseUrl)?;
    if base.scheme() != "https"
        || base.host_str().is_none()
        || !base.username().is_empty()
        || base.password().is_some()
    {
        return Err(WorkspaceRoutingError::InvalidBaseUrl);
    }
    let origin = if backend == "NO_CONSTRAINT" {
        None
    } else {
        let origin =
            Url::parse(&backend).map_err(|_| WorkspaceRoutingError::InvalidBackendOrigin)?;
        if backend.trim() != backend
            || origin.scheme() != "https"
            || origin.host_str().is_none()
            || !origin.username().is_empty()
            || origin.password().is_some()
            || origin.path() != "/"
            || origin.query().is_some()
            || origin.fragment().is_some()
        {
            return Err(WorkspaceRoutingError::InvalidBackendOrigin);
        }
        Some(origin)
    };
    if let Some(origin) = origin {
        base.set_scheme(origin.scheme())
            .map_err(|_| WorkspaceRoutingError::InvalidBackendOrigin)?;
        base.set_host(origin.host_str())
            .map_err(|_| WorkspaceRoutingError::InvalidBackendOrigin)?;
        base.set_port(origin.port())
            .map_err(|_| WorkspaceRoutingError::InvalidBackendOrigin)?;
    }
    Ok(WorkspaceRouting {
        base_url: base.as_str().trim_end_matches('/').to_owned(),
        routing_override,
    })
}

async fn read_capped_body(response: reqwest::Response) -> CodexClientResult<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_ACCOUNTS_CHECK_BODY_BYTES as u64)
    {
        return Err(WorkspaceRoutingError::ResponseTooLarge.into());
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(CodexClientError::HttpJson)?;
        if body
            .len()
            .checked_add(chunk.len())
            .is_none_or(|length| length > MAX_ACCOUNTS_CHECK_BODY_BYTES)
        {
            return Err(WorkspaceRoutingError::ResponseTooLarge.into());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use reqwest::header::HeaderMap;
    use serde_json::{Map, Value};

    use super::*;
    use crate::transport::{
        profile::CodexWireProfileState, protocol::responses::CodexResponsesRequest,
    };

    fn entry(origin: Option<&str>, routing: Option<&str>) -> AccountEntry {
        AccountEntry {
            id: "workspace".to_owned(),
            workspace_backend_origin: origin.map(str::to_owned),
            account_routing_override: routing.map(str::to_owned),
        }
    }

    #[test]
    fn replaces_only_origin_and_preserves_backend_path() {
        let routing = resolve_routing(
            entry(Some("https://gov.chatgpt.com"), Some("us_cr")),
            "https://chatgpt.com/backend-api",
        )
        .expect("valid routing");
        assert_eq!(
            routing,
            WorkspaceRouting {
                base_url: "https://gov.chatgpt.com/backend-api".to_owned(),
                routing_override: Some("us_cr".to_owned()),
            }
        );
    }

    #[test]
    fn no_constraint_keeps_effective_base_and_omits_header() {
        let routing = resolve_routing(
            entry(Some("NO_CONSTRAINT"), Some("NO_CONSTRAINT")),
            "https://chatgpt.com/backend-api",
        )
        .expect("valid routing");
        assert_eq!(routing.base_url, "https://chatgpt.com/backend-api");
        assert_eq!(routing.routing_override, None);
    }

    #[test]
    fn rejects_non_origin_or_unknown_override() {
        assert_eq!(
            resolve_routing(
                entry(Some("https://gov.chatgpt.com/backend-api"), Some("us")),
                "https://chatgpt.com/backend-api",
            ),
            Err(WorkspaceRoutingError::InvalidBackendOrigin)
        );
        assert_eq!(
            resolve_routing(
                entry(Some("https://gov.chatgpt.com"), Some("eu")),
                "https://chatgpt.com/backend-api",
            ),
            Err(WorkspaceRoutingError::InvalidRoutingOverride)
        );
    }

    #[test]
    fn emits_only_valid_codex_routing_header() {
        let mut client = CodexBackendClient::new(
            reqwest::Client::new(),
            "https://chatgpt.com/backend-api",
            CodexWireProfileState::new(Default::default()),
        );
        client.workspace_routing = Some(WorkspaceRouting {
            base_url: "https://gov.chatgpt.com/backend-api".to_owned(),
            routing_override: Some("us".to_owned()),
        });
        let mut body = Map::new();
        body.insert("model".to_owned(), Value::String("gpt-test".to_owned()));
        let request = CodexResponsesRequest::from_body(body);
        let context =
            CodexRequestContext::auxiliary("Bearer test", Some("workspace"), "request-id", None);
        for headers in [
            client
                .request_headers_for_http_response(&request, context)
                .expect("HTTP headers"),
            client
                .request_headers_for_websocket_response(&request, context)
                .expect("WebSocket headers"),
        ] {
            assert_eq!(
                headers
                    .get("x-openai-account-routing-override")
                    .expect("routing header"),
                "us"
            );
        }

        let mut headers = HeaderMap::new();
        client.insert_workspace_routing_header(&mut headers);
        assert_eq!(
            headers
                .get("x-openai-account-routing-override")
                .expect("routing header"),
            "us"
        );

        client.protocol = OpenAiUpstreamProtocol::ResponsesApi;
        let mut headers = HeaderMap::new();
        client.insert_workspace_routing_header(&mut headers);
        assert!(headers.get("x-openai-account-routing-override").is_none());
    }
}

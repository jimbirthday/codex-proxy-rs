//! 自由 HTTP 探测的管理员敏感合同；原始交换只在本次响应中返回。

use gateway_admin::model::proxies::{
    AccountProxySelection, FreeProbeCommand, HttpProbeExchange, HttpProbeHeader, HttpProbeRequest,
};
use gateway_core::account::OutboundProxy;

use super::*;
use crate::auth::SessionState;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProbeRequest {
    account_id: Option<String>,
    #[serde(default)]
    use_account_headers: bool,
    proxy_id: Option<String>,
    proxy_url: Option<String>,
    method: String,
    url: String,
    headers: Vec<ProbeHeader>,
    body_base64: String,
    timeout_seconds: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProbeHeader {
    name: String,
    value_base64: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProbeExchange {
    method: String,
    url: String,
    request_headers: Vec<ProbeHeader>,
    request_body_base64: String,
    status_code: Option<u16>,
    http_version: Option<String>,
    response_headers: Vec<ProbeHeader>,
    response_body_base64: String,
    elapsed_ms: u64,
    error: Option<String>,
}

impl ProbeRequest {
    fn into_command(self) -> Result<FreeProbeCommand, AdminError> {
        let invalid =
            || map_service_error(AdminServiceError::invalid("Base64、账号或代理参数不合法"));
        let proxy = match (self.proxy_id, self.proxy_url) {
            (Some(id), None) => AccountProxySelection::Saved(id),
            (None, Some(url)) => {
                AccountProxySelection::Url(OutboundProxy::parse(&url).map_err(|_| invalid())?)
            }
            (None, None) => AccountProxySelection::Direct,
            _ => return Err(invalid()),
        };
        Ok(FreeProbeCommand {
            account_id: self
                .account_id
                .map(ProviderAccountId::new)
                .transpose()
                .map_err(|_| invalid())?,
            use_account_headers: self.use_account_headers,
            proxy,
            request: HttpProbeRequest {
                method: self.method,
                url: self.url,
                headers: self
                    .headers
                    .into_iter()
                    .map(|header| {
                        Ok(HttpProbeHeader {
                            name: header.name,
                            value: STANDARD_BASE64
                                .decode(header.value_base64)
                                .map_err(|_| invalid())?,
                        })
                    })
                    .collect::<Result<_, AdminError>>()?,
                body: STANDARD_BASE64
                    .decode(self.body_base64)
                    .map_err(|_| invalid())?,
                timeout_seconds: self.timeout_seconds,
            },
        })
    }
}

fn encode_headers(headers: Vec<HttpProbeHeader>) -> Vec<ProbeHeader> {
    headers
        .into_iter()
        .map(|header| ProbeHeader {
            name: header.name,
            value_base64: STANDARD_BASE64.encode(header.value),
        })
        .collect()
}

impl From<HttpProbeExchange> for ProbeExchange {
    fn from(value: HttpProbeExchange) -> Self {
        Self {
            method: value.request.method,
            url: value.request.url,
            request_headers: encode_headers(value.request.headers),
            request_body_base64: STANDARD_BASE64.encode(value.request.body),
            status_code: value.status_code,
            http_version: value.http_version,
            response_headers: encode_headers(value.response_headers),
            response_body_base64: STANDARD_BASE64.encode(value.response_body),
            elapsed_ms: value.elapsed_ms,
            error: value.error,
        }
    }
}

pub(super) fn router<S>() -> Router<S>
where
    S: SessionState + Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/api/admin/accounts/free-probe", post(send::<S>))
        .layer(axum::extract::DefaultBodyLimit::disable())
}

async fn send<S>(
    auth: AdminAuth,
    State(state): State<S>,
    AdminJson(request): AdminJson<ProbeRequest>,
) -> Result<impl IntoResponse, AdminError>
where
    S: SessionState + Send + Sync,
{
    let exchange = state
        .admin_services()
        .accounts()
        .free_probe(&auth.context().mutation_context(), request.into_command()?)
        .await
        .map_err(map_service_error)?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        AdminResponse::new(
            StatusCode::OK,
            AdminEnvelope::ok(ProbeExchange::from(exchange)),
        ),
    ))
}

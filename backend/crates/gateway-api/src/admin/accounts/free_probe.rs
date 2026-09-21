//! 自由 HTTP 探测的管理员敏感合同；通过事件流交付交换进度，正文由所属管理员下载。

use gateway_admin::model::proxies::{
    AccountProxySelection, FreeProbeCommand, HttpProbeEvent, HttpProbeExchange, HttpProbeHeader,
    HttpProbeRequest,
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
    automatic_request_headers: Vec<String>,
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
            automatic_request_headers: value.automatic_request_headers,
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
        .route("/api/admin/accounts/free-probe/body", get(download::<S>))
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
    let command = request.into_command()?;
    let accounts = state.admin_services().accounts_handle();
    let context = auth.context().mutation_context();
    // 立即建立事件流，准备和上游请求的生命周期随响应流一起取消。
    let stream = futures::stream::once(async move {
        match accounts.free_probe(&context, command).await {
            Ok(session) => {
                let prepared = probe_event("prepared", serde_json::json!({ "id": session.id }));
                futures::stream::iter([prepared])
                    .chain(session.events.map(encode_probe_event))
                    .boxed()
            }
            Err(error) => futures::stream::iter([probe_event(
                "error",
                serde_json::json!({ "message": error.to_string() }),
            )])
            .boxed(),
        }
    })
    .flatten();
    let stream =
        futures::stream::iter([probe_event("connecting", serde_json::json!({}))]).chain(stream);
    Ok((
        [
            (header::CACHE_CONTROL, "no-store"),
            (HeaderName::from_static("x-accel-buffering"), "no"),
        ],
        Sse::new(stream).keep_alive(KeepAlive::default()),
    ))
}

fn encode_probe_event(event: HttpProbeEvent) -> Result<Event, Infallible> {
    match event {
        HttpProbeEvent::Headers(exchange) => probe_event(
            "headers",
            serde_json::to_value(ProbeExchange::from(*exchange)).expect("probe metadata"),
        ),
        HttpProbeEvent::Progress {
            received_bytes,
            preview,
        } => probe_event(
            "progress",
            serde_json::json!({
                "receivedBytes": received_bytes,
                "previewBase64": STANDARD_BASE64.encode(preview),
            }),
        ),
        HttpProbeEvent::Complete { elapsed_ms, error } => probe_event(
            "complete",
            serde_json::json!({ "elapsedMs": elapsed_ms, "error": error }),
        ),
    }
}

fn probe_event(name: &str, data: Value) -> Result<Event, Infallible> {
    Ok(Event::default().event(name).data(data.to_string()))
}

#[derive(Deserialize)]
struct BodyQuery {
    id: String,
}

async fn download<S>(
    auth: AdminAuth,
    State(state): State<S>,
    AdminQuery(query): AdminQuery<BodyQuery>,
) -> Result<Response, AdminError>
where
    S: SessionState + Send + Sync,
{
    let body = state
        .admin_services()
        .accounts()
        .free_probe_body(&auth.context().mutation_context(), &query.id)
        .await
        .map_err(map_service_error)?;
    // 下载本次点击时已持久化的字节快照，运行中或取消后的部分正文同样可取回。
    let length = body.byte_length();
    let stream = futures::stream::try_unfold((body, 0_u64), move |(body, offset)| async move {
        if offset >= length {
            return Ok(None);
        }
        let bytes = body
            .read(offset, (length - offset).min(64 * 1024) as usize)
            .await
            .map_err(|_| std::io::Error::other("读取探测正文失败"))?;
        if bytes.is_empty() {
            return Err(std::io::Error::other("探测正文提前结束"));
        }
        let next = offset + bytes.len() as u64;
        Ok(Some((bytes, (body, next))))
    });
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
            (header::CONTENT_LENGTH, length.to_string()),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"response-body.bin\"".to_owned(),
            ),
            (header::CACHE_CONTROL, "no-store".to_owned()),
        ],
        Body::from_stream(stream),
    )
        .into_response())
}

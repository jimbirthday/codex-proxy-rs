//! 使用与 Provider 请求一致的显式代理协议，执行有超时和响应大小限制的出口测试。

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use futures::StreamExt as _;
use gateway_admin::model::{
    AdminError,
    proxies::{HttpProbeEvent, HttpProbeExchange, HttpProbeSession},
};
use gateway_admin::ports::proxy::HttpProbeBody;
use gateway_admin::{model::proxies::ProxyTestResult, ports::proxy::ProxyProbe};
use gateway_core::account::OutboundProxy;
use serde::Deserialize;
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _};

struct ProbeBodyFile {
    file: tokio::sync::Mutex<tokio::fs::File>,
    length: std::sync::atomic::AtomicU64,
    finished: std::sync::Mutex<Option<Instant>>,
}

#[async_trait]
impl HttpProbeBody for ProbeBodyFile {
    fn byte_length(&self) -> u64 {
        self.length.load(std::sync::atomic::Ordering::Acquire)
    }

    fn finished_at(&self) -> Option<Instant> {
        *self.finished.lock().expect("probe finish mutex")
    }

    async fn read(&self, offset: u64, length: usize) -> Result<Vec<u8>, AdminError> {
        let length = length
            .min(64 * 1024)
            .min(self.byte_length().saturating_sub(offset).min(64 * 1024) as usize);
        let mut bytes = vec![0; length];
        let mut file = self.file.lock().await;
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|_| AdminError::internal("读取探测正文失败"))?;
        file.read_exact(&mut bytes)
            .await
            .map_err(|_| AdminError::internal("读取探测正文失败"))?;
        Ok(bytes)
    }
}

impl ProbeBodyFile {
    async fn append(&self, bytes: &[u8]) -> Result<u64, AdminError> {
        let mut file = self.file.lock().await;
        file.seek(std::io::SeekFrom::End(0))
            .await
            .map_err(|_| AdminError::internal("保存探测正文失败"))?;
        file.write_all(bytes)
            .await
            .map_err(|_| AdminError::internal("保存探测正文失败，请检查临时磁盘空间"))?;
        file.flush()
            .await
            .map_err(|_| AdminError::internal("保存探测正文失败，请检查临时磁盘空间"))?;
        Ok(self
            .length
            .fetch_add(bytes.len() as u64, std::sync::atomic::Ordering::Release)
            + bytes.len() as u64)
    }
}

/// 事件流被取消或断开时同样终结正文生命周期，不能留下一份永不过期的运行记录。
struct ProbeWriteGuard(Arc<ProbeBodyFile>);
impl Drop for ProbeWriteGuard {
    fn drop(&mut self) {
        *self.0.finished.lock().expect("probe finish mutex") = Some(Instant::now());
    }
}

struct ProbeStreamState {
    outbound: Option<(reqwest::Client, reqwest::Request)>,
    exchange: Option<HttpProbeExchange>,
    response: Option<reqwest::Response>,
    body: Arc<ProbeBodyFile>,
    guard: Option<ProbeWriteGuard>,
    started: Instant,
    preview_bytes: usize,
    error: Option<String>,
    done: bool,
}

impl ProbeStreamState {
    async fn next(mut self) -> Option<(HttpProbeEvent, Self)> {
        if self.done {
            return None;
        }
        if let Some((client, outbound)) = self.outbound.take() {
            let mut exchange = self.exchange.take().expect("initial exchange");
            match client.execute(outbound).await {
                Ok(response) => {
                    exchange.status_code = Some(response.status().as_u16());
                    exchange.http_version = Some(format!("{:?}", response.version()));
                    exchange.response_headers = response
                        .headers()
                        .iter()
                        .map(
                            |(name, value)| gateway_admin::model::proxies::HttpProbeHeader {
                                name: name.to_string(),
                                value: value.as_bytes().to_vec(),
                            },
                        )
                        .collect();
                    self.response = Some(response);
                }
                Err(error) => {
                    self.error = Some(
                        if error.is_timeout() {
                            "请求超时"
                        } else {
                            "请求发送失败，请检查 URL、证书、代理和 HTTP 报头"
                        }
                        .to_owned(),
                    );
                }
            }
            exchange.elapsed_ms =
                u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
            exchange.error = self.error.clone();
            return Some((HttpProbeEvent::Headers(Box::new(exchange)), self));
        }
        if let Some(response) = self.response.as_mut() {
            match response.chunk().await {
                Ok(Some(bytes)) => match self.body.append(&bytes).await {
                    Ok(received_bytes) => {
                        let count = bytes
                            .len()
                            .min((64 * 1024_usize).saturating_sub(self.preview_bytes));
                        self.preview_bytes += count;
                        return Some((
                            HttpProbeEvent::Progress {
                                received_bytes,
                                preview: bytes[..count].to_vec(),
                            },
                            self,
                        ));
                    }
                    Err(error) => self.error = Some(error.to_string()),
                },
                Ok(None) => {}
                Err(error) => {
                    self.error = Some(
                        if error.is_timeout() {
                            "响应读取超时，正文不完整"
                        } else {
                            "响应读取失败，正文不完整"
                        }
                        .to_owned(),
                    )
                }
            }
        }
        self.response = None;
        self.done = true;
        drop(self.guard.take());
        Some((
            HttpProbeEvent::Complete {
                elapsed_ms: u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
                error: self.error.take(),
            },
            self,
        ))
    }
}

enum ProbeStrategy {
    Single(String),
    Dual {
        ipv4_endpoint: String,
        ipv6_endpoint: String,
    },
}

pub struct HttpProxyProbe {
    strategy: ProbeStrategy,
    build_client: Arc<ProxyClientBuilder>,
}

type ProxyClientBuilder =
    dyn Fn(reqwest::ClientBuilder) -> Result<reqwest::Client, &'static str> + Send + Sync;

fn proxy_connection_failure(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        return "代理连接超时";
    }
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    let mut fallback = "代理连接失败，请检查地址、认证和网络";
    // reqwest 未公开 SOCKS 错误类型，只匹配锁定依赖的固定错误文本。
    // 原错误可能包含 URL 或认证数据，绝不能直接作为管理端提示返回。
    for _ in 0..16 {
        let Some(cause) = source else { break };
        let message = cause.to_string().to_ascii_lowercase();
        match message.as_str() {
            "socks error: credentials not accepted" => {
                return "SOCKS5 认证被代理拒绝，请检查用户名、密码及服务商授权条件";
            }
            "socks error: server does not support user/pass authentication"
            | "socks error: server implements authentication incorrectly" => {
                return "SOCKS5 认证方式协商失败，请核对代理协议和端口";
            }
            "socks error: connection not allowed" => {
                return "SOCKS5 代理拒绝连接检测目标，请检查服务商访问规则";
            }
            "socks error: network unreachable" | "socks error: host unreachable" => {
                return "SOCKS5 代理报告目标不可达，请检查出口资源和目标域名解析";
            }
            "socks error: connection refused" => {
                return "SOCKS5 代理报告目标连接被拒绝";
            }
            "socks error: general server failure" => return "SOCKS5 代理报告服务端故障",
            "socks error: command not supported" | "socks error: address type not supported" => {
                return "SOCKS5 代理不支持本次连接命令或目标地址类型";
            }
            "socks error: ttl expired" => return "SOCKS5 代理报告连接存活时间已耗尽",
            "socks error: failed parsing server response" => {
                return "SOCKS5 握手响应格式错误，请核对代理协议和端口";
            }
            "socks error: io error during socks handshake" => {
                return "SOCKS5 握手期间连接中断，请检查代理服务和网络";
            }
            "dns error" | "error resolving for socks proxy" => {
                return "代理或检测目标域名解析失败";
            }
            _ => {}
        }
        if message.contains("certificate") {
            return "代理测试的 TLS 证书校验失败，请检查证书信任配置";
        }
        if message.contains("tls") || message.contains("ssl") {
            fallback = "代理测试的 TLS 握手失败，请检查代理转发链路和目标访问限制";
        } else if message.contains("unexpected eof") || message.contains("connection reset") {
            fallback = "连接被提前关闭，未取得出口检测响应，请检查代理转发链路";
        }
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            match io.kind() {
                std::io::ErrorKind::ConnectionRefused => {
                    return "代理 TCP 连接被拒绝，请核对地址和端口";
                }
                std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset => {
                    fallback = "连接被提前关闭，未取得出口检测响应，请检查代理转发链路";
                }
                _ => {}
            }
        }
        source = cause.source();
    }
    fallback
}

impl Default for HttpProxyProbe {
    fn default() -> Self {
        // 分别向 IPv4 和 IPv6 专用端点并发探测，以获取真实的双栈出口地址。
        Self::new_dual(
            "https://api.ipify.org?format=json",
            "https://api6.ipify.org?format=json",
        )
    }
}

impl HttpProxyProbe {
    #[must_use]
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            strategy: ProbeStrategy::Single(endpoint.into()),
            build_client: Arc::new(|builder| builder.build().map_err(|_| "无法创建代理连接")),
        }
    }

    #[must_use]
    pub fn new_dual(ipv4_endpoint: impl Into<String>, ipv6_endpoint: impl Into<String>) -> Self {
        Self {
            strategy: ProbeStrategy::Dual {
                ipv4_endpoint: ipv4_endpoint.into(),
                ipv6_endpoint: ipv6_endpoint.into(),
            },
            build_client: Arc::new(|builder| builder.build().map_err(|_| "无法创建代理连接")),
        }
    }

    /// 由组合根注入与 Provider 请求一致的证书信任策略。
    #[must_use]
    pub fn with_client_builder<E>(
        mut self,
        build: impl Fn(reqwest::ClientBuilder) -> Result<reqwest::Client, E> + Send + Sync + 'static,
    ) -> Self {
        self.build_client = Arc::new(move |builder| {
            build(builder).map_err(|_| "无法创建代理连接，请检查证书信任配置")
        });
        self
    }

    async fn exit_ip_at(
        &self,
        proxy: &OutboundProxy,
        endpoint: &str,
    ) -> Result<IpAddr, &'static str> {
        let proxy = reqwest::Proxy::all(proxy.expose_url()).map_err(|_| "代理地址不合法")?;
        let builder = reqwest::Client::builder()
            .no_proxy()
            .proxy(proxy)
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(12))
            .redirect(reqwest::redirect::Policy::none());
        let client = (self.build_client)(builder)?;
        let mut response = client
            .get(endpoint)
            .send()
            .await
            .map_err(|error| proxy_connection_failure(&error))?;
        if !response.status().is_success() {
            return Err(
                if response.status() == reqwest::StatusCode::PROXY_AUTHENTICATION_REQUIRED {
                    "代理认证失败"
                } else {
                    "出口检测服务返回错误状态"
                },
            );
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| "出口检测响应读取失败")?
        {
            if body.len() + chunk.len() > 1024 {
                return Err("出口检测响应过大");
            }
            body.extend_from_slice(&chunk);
        }
        #[derive(Deserialize)]
        struct Response {
            ip: IpAddr,
        }
        serde_json::from_slice::<Response>(&body)
            .map(|response| response.ip)
            .map_err(|_| "出口检测响应不合法")
    }
}

#[async_trait]
impl ProxyProbe for HttpProxyProbe {
    async fn send_http(
        &self,
        proxy: Option<&OutboundProxy>,
        mut request: gateway_admin::model::proxies::HttpProbeRequest,
    ) -> Result<HttpProbeSession, AdminError> {
        use gateway_admin::model::{AdminError, proxies::HttpProbeHeader};
        use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
        let invalid = || AdminError::invalid("请求方法、URL 或报头不符合 HTTP 格式");
        let method =
            reqwest::Method::from_bytes(request.method.as_bytes()).map_err(|_| invalid())?;
        let url = reqwest::Url::parse(&request.url).map_err(|_| invalid())?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err(invalid());
        }
        let mut headers = HeaderMap::new();
        for header in &request.headers {
            headers
                .try_append(
                    HeaderName::from_bytes(header.name.as_bytes()).map_err(|_| invalid())?,
                    HeaderValue::from_bytes(&header.value).map_err(|_| invalid())?,
                )
                .map_err(|_| invalid())?;
        }
        let mut automatic_request_headers = Vec::new();
        // 标记自动生成字段，复用后继续随 URL 和正文变化；手填覆盖始终保持原值。
        if !headers.contains_key("host") {
            automatic_request_headers.push("host".to_owned());
            let host = match url.port() {
                Some(port) => format!("{}:{port}", url.host().expect("validated host")),
                None => url.host().expect("validated host").to_string(),
            };
            headers.insert("host", HeaderValue::from_str(&host).map_err(|_| invalid())?);
        }
        if !headers.contains_key("accept") {
            automatic_request_headers.push("accept".to_owned());
            headers.insert("accept", HeaderValue::from_static("*/*"));
        }
        if !headers.contains_key("content-length") && !headers.contains_key("transfer-encoding") {
            automatic_request_headers.push("content-length".to_owned());
            headers.insert(
                "content-length",
                HeaderValue::from_str(&request.body.len().to_string()).map_err(|_| invalid())?,
            );
        }
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none());
        if request.timeout_seconds > 0 {
            let timeout = Duration::from_secs(request.timeout_seconds);
            if Instant::now().checked_add(timeout).is_none() {
                return Err(AdminError::invalid("超时秒数超出系统支持范围"));
            }
            builder = builder.timeout(timeout);
        }
        if let Some(proxy) = proxy {
            builder = builder.proxy(
                reqwest::Proxy::all(proxy.expose_url())
                    .map_err(|_| AdminError::invalid("代理地址不合法"))?,
            );
        }
        let client = (self.build_client)(builder).map_err(AdminError::invalid)?;
        let outbound = client
            .request(method, url)
            .headers(headers)
            .body(request.body.clone())
            .build()
            .map_err(|_| invalid())?;
        request.url = outbound.url().to_string();
        request.headers = outbound
            .headers()
            .iter()
            .map(|(name, value)| HttpProbeHeader {
                name: name.to_string(),
                value: value.as_bytes().to_vec(),
            })
            .collect();
        // 匿名临时文件权限由 tempfile 收紧，最后一个句柄释放时由操作系统删除。
        let file = tokio::task::spawn_blocking(tempfile::tempfile)
            .await
            .map_err(|_| AdminError::internal("创建探测临时文件失败"))?
            .map_err(|_| AdminError::internal("创建探测临时文件失败"))?;
        let body = Arc::new(ProbeBodyFile {
            file: tokio::sync::Mutex::new(tokio::fs::File::from_std(file)),
            length: std::sync::atomic::AtomicU64::new(0),
            finished: std::sync::Mutex::new(None),
        });
        let stream_state = ProbeStreamState {
            outbound: Some((client, outbound)),
            exchange: Some(HttpProbeExchange {
                request,
                status_code: None,
                http_version: None,
                response_headers: Vec::new(),
                automatic_request_headers,
                elapsed_ms: 0,
                error: None,
                turn_state_length: None,
                turn_state_stored: false,
            }),
            response: None,
            body: Arc::clone(&body),
            guard: Some(ProbeWriteGuard(Arc::clone(&body))),
            started: Instant::now(),
            preview_bytes: 0,
            error: None,
            done: false,
        };
        Ok(HttpProbeSession {
            events: futures::stream::unfold(stream_state, ProbeStreamState::next).boxed(),
            body,
        })
    }

    async fn test(&self, proxy: &OutboundProxy) -> ProxyTestResult {
        let started = Instant::now();
        let timeout_limit = Duration::from_secs(15);

        match &self.strategy {
            ProbeStrategy::Single(endpoint) => {
                let result =
                    tokio::time::timeout(timeout_limit, self.exit_ip_at(proxy, endpoint)).await;
                let result = result.unwrap_or(Err("代理连接超时"));
                let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

                match result {
                    Ok(ip) => {
                        let (exit_ipv4, exit_ipv6) = match ip {
                            IpAddr::V4(v4) => (Some(v4), None),
                            IpAddr::V6(v6) => (None, Some(v6)),
                        };
                        ProxyTestResult {
                            success: true,
                            latency_ms,
                            exit_ip: Some(ip),
                            exit_ipv4,
                            exit_ipv6,
                            message: "连接成功".to_owned(),
                        }
                    }
                    Err(err) => ProxyTestResult {
                        success: false,
                        latency_ms,
                        exit_ip: None,
                        exit_ipv4: None,
                        exit_ipv6: None,
                        message: err.to_owned(),
                    },
                }
            }
            ProbeStrategy::Dual {
                ipv4_endpoint,
                ipv6_endpoint,
            } => {
                let probe_dual = async {
                    tokio::join!(
                        self.exit_ip_at(proxy, ipv4_endpoint),
                        self.exit_ip_at(proxy, ipv6_endpoint),
                    )
                };
                let result = tokio::time::timeout(timeout_limit, probe_dual).await;
                let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

                match result {
                    Ok((res_v4, res_v6)) => {
                        let exit_ipv4: Option<Ipv4Addr> = match res_v4 {
                            Ok(IpAddr::V4(v4)) => Some(v4),
                            _ => None,
                        };
                        let exit_ipv6: Option<Ipv6Addr> = match res_v6 {
                            Ok(IpAddr::V6(v6)) => Some(v6),
                            _ => None,
                        };

                        if exit_ipv4.is_some() && exit_ipv6.is_some() {
                            ProxyTestResult {
                                success: true,
                                latency_ms,
                                exit_ip: exit_ipv4.map(IpAddr::V4),
                                exit_ipv4,
                                exit_ipv6,
                                message: "连接成功（双栈可用）".to_owned(),
                            }
                        } else if let Some(v4) = exit_ipv4 {
                            ProxyTestResult {
                                success: true,
                                latency_ms,
                                exit_ip: Some(IpAddr::V4(v4)),
                                exit_ipv4: Some(v4),
                                exit_ipv6: None,
                                message: "连接成功（仅 IPv4）".to_owned(),
                            }
                        } else if let Some(v6) = exit_ipv6 {
                            ProxyTestResult {
                                success: true,
                                latency_ms,
                                exit_ip: Some(IpAddr::V6(v6)),
                                exit_ipv4: None,
                                exit_ipv6: Some(v6),
                                message: "连接成功（仅 IPv6）".to_owned(),
                            }
                        } else {
                            let message = res_v4
                                .err()
                                .or(res_v6.err())
                                .unwrap_or("代理连接失败")
                                .to_owned();
                            ProxyTestResult {
                                success: false,
                                latency_ms,
                                exit_ip: None,
                                exit_ipv4: None,
                                exit_ipv6: None,
                                message,
                            }
                        }
                    }
                    Err(_) => ProxyTestResult {
                        success: false,
                        latency_ms,
                        exit_ip: None,
                        exit_ipv4: None,
                        exit_ipv6: None,
                        message: "代理连接超时".to_owned(),
                    },
                }
            }
        }
    }
}

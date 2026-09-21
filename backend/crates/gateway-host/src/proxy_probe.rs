//! 使用与 Provider 请求一致的显式代理协议，执行有超时和响应大小限制的出口测试。

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use gateway_admin::{model::proxies::ProxyTestResult, ports::proxy::ProxyProbe};
use gateway_core::account::OutboundProxy;
use serde::Deserialize;

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
        let mut response = client.get(endpoint).send().await.map_err(|error| {
            if error.is_timeout() {
                "代理连接超时"
            } else {
                "代理连接失败，请检查地址、认证和网络"
            }
        })?;
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
    ) -> Result<gateway_admin::model::proxies::HttpProbeExchange, gateway_admin::model::AdminError>
    {
        use gateway_admin::model::{
            AdminError,
            proxies::{HttpProbeExchange, HttpProbeHeader},
        };
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
        // 显式补齐 HTTP 客户端通常自动生成的字段，使交换详情可以检查实际发送值。
        if !headers.contains_key("host") {
            let host = match url.port() {
                Some(port) => format!("{}:{port}", url.host().expect("validated host")),
                None => url.host().expect("validated host").to_string(),
            };
            headers.insert("host", HeaderValue::from_str(&host).map_err(|_| invalid())?);
        }
        if !headers.contains_key("accept") {
            headers.insert("accept", HeaderValue::from_static("*/*"));
        }
        if !headers.contains_key("content-length") && !headers.contains_key("transfer-encoding") {
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
        let started = Instant::now();
        let mut exchange = HttpProbeExchange {
            request,
            status_code: None,
            http_version: None,
            response_headers: Vec::new(),
            response_body: Vec::new(),
            elapsed_ms: 0,
            error: None,
        };
        match client.execute(outbound).await {
            Ok(mut response) => {
                exchange.status_code = Some(response.status().as_u16());
                exchange.http_version = Some(format!("{:?}", response.version()));
                exchange.response_headers = response
                    .headers()
                    .iter()
                    .map(|(name, value)| HttpProbeHeader {
                        name: name.to_string(),
                        value: value.as_bytes().to_vec(),
                    })
                    .collect();
                loop {
                    match response.chunk().await {
                        Ok(Some(bytes)) => exchange.response_body.extend_from_slice(&bytes),
                        Ok(None) => break,
                        Err(error) => {
                            exchange.error = Some(
                                if error.is_timeout() {
                                    "响应读取超时，正文不完整"
                                } else {
                                    "响应读取失败，正文不完整"
                                }
                                .to_owned(),
                            );
                            break;
                        }
                    }
                }
            }
            Err(error) => {
                // reqwest 的错误文本可能携带 URL 凭据，不能直接返回或记录。
                exchange.error = Some(
                    if error.is_timeout() {
                        "请求超时"
                    } else {
                        "请求发送失败，请检查 URL、证书、代理和 HTTP 报头"
                    }
                    .to_owned(),
                );
            }
        }
        exchange.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        Ok(exchange)
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

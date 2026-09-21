use gateway_admin::ports::proxy::ProxyProbe;
use gateway_core::account::OutboundProxy;
use gateway_host::proxy_probe::HttpProxyProbe;
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{any, header, path},
};

#[tokio::test]
async fn proxy_probe_supports_ipv4_and_ipv6_proxies_and_exit_addresses() {
    for (listen_address, exit_ip) in [
        ("127.0.0.1:0", "203.0.113.8"),
        ("127.0.0.1:0", "2001:db8::8"),
        ("[::1]:0", "203.0.113.8"),
        ("[::1]:0", "2001:db8::8"),
    ] {
        let listener = std::net::TcpListener::bind(listen_address).unwrap();
        let proxy_server = MockServer::builder().listener(listener).start().await;
        Mock::given(header("proxy-authorization", "Basic dXNlcjpwYXNzd29yZA=="))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ip": exit_ip})))
            .expect(1)
            .mount(&proxy_server)
            .await;
        let proxy =
            OutboundProxy::parse(&format!("http://user:password@{}", proxy_server.address()))
                .unwrap();
        let result = HttpProxyProbe::new("http://unresolvable.invalid/ip")
            .test(&proxy)
            .await;
        assert!(
            result.success,
            "{listen_address} -> {exit_ip}: {}",
            result.message
        );
        assert_eq!(result.exit_ip.unwrap().to_string(), exit_ip);
    }
}

#[tokio::test]
async fn proxy_probe_rejects_auth_errors_redirects_and_invalid_or_oversized_responses() {
    for response in [
        ResponseTemplate::new(407),
        ResponseTemplate::new(302).insert_header("Location", "http://127.0.0.1/"),
        ResponseTemplate::new(200).set_body_json(json!({"ip": "not-an-ip"})),
        ResponseTemplate::new(200).set_body_string("a".repeat(1025)),
    ] {
        let proxy_server = MockServer::start().await;
        Mock::given(any())
            .respond_with(response)
            .expect(1)
            .mount(&proxy_server)
            .await;
        let result = HttpProxyProbe::new("http://unresolvable.invalid/ip")
            .test(&OutboundProxy::parse(&proxy_server.uri()).unwrap())
            .await;
        assert!(!result.success);
        assert!(result.exit_ip.is_none());
    }
}

#[tokio::test]
async fn unavailable_proxy_never_falls_back_to_direct_connection() {
    let target = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ip":"203.0.113.8"})))
        .expect(0)
        .mount(&target)
        .await;
    let unused = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = OutboundProxy::parse(&format!("http://{}", unused.local_addr().unwrap())).unwrap();
    drop(unused);
    let result = HttpProxyProbe::new(target.uri()).test(&proxy).await;
    assert!(!result.success);
}

#[tokio::test]
async fn invalid_certificate_configuration_should_not_fall_back_or_expose_details() {
    let target = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&target)
        .await;
    let result = HttpProxyProbe::new(target.uri())
        .with_client_builder(|_| Err("private-certificate-path"))
        .test(&OutboundProxy::parse(&target.uri()).unwrap())
        .await;
    assert!(!result.success);
    assert!(!result.message.contains("private-certificate-path"));
}

#[tokio::test]
async fn dual_stack_proxy_probe_reports_both_addresses_when_available() {
    let proxy_server = MockServer::start().await;
    Mock::given(path("/v4"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ip": "203.0.113.8"})))
        .expect(1)
        .mount(&proxy_server)
        .await;

    Mock::given(path("/v6"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ip": "2001:db8::8"})))
        .expect(1)
        .mount(&proxy_server)
        .await;

    let proxy = OutboundProxy::parse(&proxy_server.uri()).unwrap();
    let result = HttpProxyProbe::new_dual(
        format!("{}/v4", proxy_server.uri()),
        format!("{}/v6", proxy_server.uri()),
    )
    .test(&proxy)
    .await;

    assert!(result.success);
    assert_eq!(result.exit_ipv4.unwrap().to_string(), "203.0.113.8");
    assert_eq!(result.exit_ipv6.unwrap().to_string(), "2001:db8::8");
    assert!(result.message.contains("双栈可用"));
}

#[tokio::test]
async fn dual_stack_proxy_probe_reports_single_stack_when_only_one_succeeds() {
    let proxy_server = MockServer::start().await;
    Mock::given(path("/v4"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ip": "203.0.113.8"})))
        .expect(1)
        .mount(&proxy_server)
        .await;

    Mock::given(path("/v6"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&proxy_server)
        .await;

    let proxy = OutboundProxy::parse(&proxy_server.uri()).unwrap();
    let result = HttpProxyProbe::new_dual(
        format!("{}/v4", proxy_server.uri()),
        format!("{}/v6", proxy_server.uri()),
    )
    .test(&proxy)
    .await;

    assert!(result.success);
    assert_eq!(result.exit_ipv4.unwrap().to_string(), "203.0.113.8");
    assert!(result.exit_ipv6.is_none());
    assert!(result.message.contains("仅 IPv4"));
}

#[tokio::test]
async fn free_probe_preserves_binary_body_duplicate_headers_and_error_responses() {
    use gateway_admin::model::proxies::{HttpProbeHeader, HttpProbeRequest};
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(418)
                .append_header("set-cookie", "a=1")
                .append_header("set-cookie", "b=2")
                .set_body_bytes(vec![0, 255, 128, 10]),
        )
        .expect(1)
        .mount(&server)
        .await;
    let request = HttpProbeRequest {
        method: "PATCH".to_owned(),
        url: format!("{}/custom?key=value", server.uri()),
        headers: vec![
            HttpProbeHeader {
                name: "x-custom".to_owned(),
                value: b"first".to_vec(),
            },
            HttpProbeHeader {
                name: "x-custom".to_owned(),
                value: b"second".to_vec(),
            },
            HttpProbeHeader {
                name: "authorization".to_owned(),
                value: b"Bearer synthetic-test".to_vec(),
            },
        ],
        body: vec![255, 0, 128],
        timeout_seconds: 5,
    };
    let exchange = HttpProxyProbe::default()
        .send_http(None, request)
        .await
        .unwrap();
    let (exchange, response_body, _) = collect_probe(exchange).await;
    assert_eq!(exchange.status_code, Some(418));
    assert!(exchange.error.is_none());
    assert_eq!(response_body, [0, 255, 128, 10]);
    assert_eq!(
        exchange
            .response_headers
            .iter()
            .filter(|h| h.name == "set-cookie")
            .count(),
        2
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests[0].body, [255, 0, 128]);
    assert_eq!(requests[0].headers.get_all("x-custom").iter().count(), 2);
    assert_eq!(
        requests[0].headers.get("authorization").unwrap(),
        "Bearer synthetic-test"
    );
    for header in &exchange.request.headers {
        assert!(
            requests[0]
                .headers
                .get_all(&header.name)
                .iter()
                .any(|value| value.as_bytes() == header.value)
        );
    }
}

#[tokio::test]
async fn free_probe_uses_selected_proxy_and_returns_redirect_without_following() {
    use gateway_admin::model::proxies::HttpProbeRequest;
    let proxy_server = MockServer::start().await;
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", "http://unresolvable.invalid/next")
                .set_body_string("redirect body"),
        )
        .expect(1)
        .mount(&proxy_server)
        .await;
    let proxy = OutboundProxy::parse(&proxy_server.uri()).unwrap();
    let exchange = HttpProxyProbe::default()
        .send_http(
            Some(&proxy),
            HttpProbeRequest {
                method: "GET".to_owned(),
                url: "http://unresolvable.invalid/custom".to_owned(),
                headers: Vec::new(),
                body: Vec::new(),
                timeout_seconds: 5,
            },
        )
        .await
        .unwrap();
    let (exchange, response_body, _) = collect_probe(exchange).await;
    assert_eq!(exchange.status_code, Some(302));
    assert_eq!(response_body, b"redirect body");
    assert!(exchange.error.is_none());
}

#[tokio::test]
async fn free_probe_read_timeout_preserves_response_headers_and_partial_body() {
    use gateway_admin::model::proxies::HttpProbeRequest;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        assert!(socket.read(&mut request).await.unwrap() > 0);
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nX-Observed: yes\r\n\r\npartial")
            .await
            .unwrap();
        std::future::pending::<()>().await;
    });
    let exchange = HttpProxyProbe::default()
        .send_http(
            None,
            HttpProbeRequest {
                method: "GET".to_owned(),
                url: format!("http://{address}/"),
                headers: Vec::new(),
                body: Vec::new(),
                timeout_seconds: 1,
            },
        )
        .await
        .unwrap();
    let (exchange, response_body, _) = collect_probe(exchange).await;
    server.abort();
    assert_eq!(exchange.status_code, Some(200));
    assert_eq!(response_body, b"partial");
    assert!(
        exchange
            .response_headers
            .iter()
            .any(|header| header.name == "x-observed" && header.value == b"yes")
    );
    assert_eq!(exchange.error.as_deref(), Some("响应读取超时，正文不完整"));
}

async fn collect_probe(
    mut session: gateway_admin::model::proxies::HttpProbeSession,
) -> (
    gateway_admin::model::proxies::HttpProbeExchange,
    Vec<u8>,
    Vec<u8>,
) {
    use futures::StreamExt as _;
    use gateway_admin::model::proxies::HttpProbeEvent;
    let mut exchange = None;
    let mut preview = Vec::new();
    while let Some(event) = session.events.next().await {
        match event {
            HttpProbeEvent::Headers(value) => exchange = Some(*value),
            HttpProbeEvent::Progress { preview: chunk, .. } => preview.extend(chunk),
            HttpProbeEvent::Complete { elapsed_ms, error } => {
                let value = exchange.as_mut().unwrap();
                value.elapsed_ms = elapsed_ms;
                value.error = error;
            }
        }
    }
    assert!(session.body.finished_at().is_some());
    let mut body = Vec::new();
    while (body.len() as u64) < session.body.byte_length() {
        body.extend(session.body.read(body.len() as u64, 65536).await.unwrap());
    }
    (exchange.unwrap(), body, preview)
}

#[tokio::test]
async fn free_probe_large_response_keeps_full_file_and_bounded_preview() {
    use gateway_admin::model::proxies::{HttpProbeHeader, HttpProbeRequest};
    let server = MockServer::start().await;
    let body = vec![171_u8; 512 * 1024];
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
        .mount(&server)
        .await;
    let session = HttpProxyProbe::default()
        .send_http(
            None,
            HttpProbeRequest {
                method: "POST".to_owned(),
                url: server.uri(),
                headers: vec![HttpProbeHeader {
                    name: "Host".to_owned(),
                    value: b"custom.invalid".to_vec(),
                }],
                body: vec![1, 2],
                timeout_seconds: 0,
            },
        )
        .await
        .unwrap();
    assert!(server.received_requests().await.unwrap().is_empty());
    let (exchange, full, preview) = collect_probe(session).await;
    assert_eq!(full, body);
    assert_eq!(preview, body[..65536]);
    assert!(
        !exchange
            .automatic_request_headers
            .iter()
            .any(|name| name == "host")
    );
    assert!(
        exchange
            .automatic_request_headers
            .iter()
            .any(|name| name == "content-length")
    );
}

#[tokio::test]
async fn free_probe_dropping_stream_closes_upstream_and_retains_partial_body() {
    use futures::StreamExt as _;
    use gateway_admin::model::proxies::{HttpProbeEvent, HttpProbeRequest};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        assert!(socket.read(&mut request).await.unwrap() > 0);
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100000\r\n\r\npartial")
            .await
            .unwrap();
        assert_eq!(socket.read(&mut request).await.unwrap(), 0);
    });
    let mut session = HttpProxyProbe::default()
        .send_http(
            None,
            HttpProbeRequest {
                method: "GET".to_owned(),
                url: format!("http://{address}/"),
                headers: Vec::new(),
                body: Vec::new(),
                timeout_seconds: 0,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        session.events.next().await,
        Some(HttpProbeEvent::Headers(_))
    ));
    assert!(matches!(
        session.events.next().await,
        Some(HttpProbeEvent::Progress { .. })
    ));
    drop(session.events);
    tokio::time::timeout(std::time::Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
    assert!(session.body.finished_at().is_some());
    assert_eq!(session.body.read(0, 65536).await.unwrap(), b"partial");
}

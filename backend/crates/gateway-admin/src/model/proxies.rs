//! 可复用的账号出口配置，以及脱敏后的连通性测试结果。

use chrono::{DateTime, Utc};
use gateway_core::account::OutboundProxy;

use super::{PageSize, Revision, account_groups::AccountGroupRef};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountProxySelection {
    Direct,
    Url(OutboundProxy),
    Saved(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportProxyBinding {
    pub id: String,
    pub proxy: OutboundProxy,
}

#[derive(Debug, Clone)]
pub struct ProxyListQuery {
    pub page: u32,
    pub page_size: PageSize,
    pub search: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyAccountRef {
    pub id: String,
    pub name: String,
    pub email: Option<String>,
    pub provider_kind: String,
    pub authentication_kind: String,
    pub plan_type: Option<String>,
    pub plan_type_display: Option<String>,
    pub groups: Vec<AccountGroupRef>,
    pub enabled: bool,
}

/// 按代理查询关联账号，分页与搜索均在存储层执行。
#[derive(Debug, Clone)]
pub struct ProxyAccountListQuery {
    pub proxy_id: String,
    pub page: u32,
    pub page_size: PageSize,
    pub search: String,
}

#[derive(Debug, Clone)]
pub struct ProxyAccountPage {
    pub items: Vec<ProxyAccountRef>,
    pub total: u64,
    pub page: u32,
    pub page_size: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyTestResult {
    pub success: bool,
    pub latency_ms: u64,
    pub exit_ip: Option<std::net::IpAddr>,
    pub exit_ipv4: Option<std::net::Ipv4Addr>,
    pub exit_ipv6: Option<std::net::Ipv6Addr>,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct ProxyRecord {
    pub location: Option<gateway_core::account::RequestLocation>,
    pub id: String,
    pub name: String,
    pub proxy: OutboundProxy,
    pub revision: Revision,
    pub account_count: u64,
    pub last_test_at: Option<DateTime<Utc>>,
    pub last_test: Option<ProxyTestResult>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct ProxyPage {
    pub items: Vec<ProxyRecord>,
    pub total: u64,
    pub page: u32,
    pub page_size: u16,
}

#[derive(Debug, Clone)]
pub struct NewProxy {
    pub location: Option<gateway_core::account::RequestLocation>,
    pub name: String,
    pub proxy: OutboundProxy,
}

#[derive(Debug, Clone)]
pub struct UpdateProxy {
    /// 外层为空保留配置，内层为空恢复全局继承。
    pub location: Option<Option<gateway_core::account::RequestLocation>>,
    pub id: String,
    pub revision: Revision,
    pub name: String,
    pub proxy: Option<OutboundProxy>,
}

#[derive(Debug, Clone)]
pub struct ProxyMutation {
    pub config_revision: Revision,
    pub record: ProxyRecord,
}

/// 仅在管理员显式探测的请求生命周期内持有原始字节，不实现 Debug。
#[derive(Clone)]
pub struct HttpProbeHeader {
    pub name: String,
    pub value: Vec<u8>,
}

pub struct HttpProbeRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<HttpProbeHeader>,
    pub body: Vec<u8>,
    /// 零表示不设置超时，由管理员主动取消。
    pub timeout_seconds: u64,
}

pub struct HttpProbeExchange {
    pub request: HttpProbeRequest,
    pub status_code: Option<u16>,
    pub http_version: Option<String>,
    pub response_headers: Vec<HttpProbeHeader>,
    pub automatic_request_headers: Vec<String>,
    pub elapsed_ms: u64,
    /// 读取失败仍返回已收到的报头和正文，并明确标明不完整。
    pub error: Option<String>,
    /// WebSocket 预热从握手或 metadata 帧观察到的 State 长度；HTTP 探测为 `None`。
    pub turn_state_length: Option<u16>,
    /// 合格 State 已写入当前账号与模型的进程内缓存。
    pub turn_state_stored: bool,
}

pub struct FreeProbeCommand {
    pub account_id: Option<gateway_core::account::ProviderAccountId>,
    pub use_account_headers: bool,
    pub proxy: AccountProxySelection,
    pub request: HttpProbeRequest,
    pub mode: FreeProbeMode,
    pub model: Option<String>,
}

/// 自由探测的发送方式。省略时保持既有 HTTP 合同，页面默认选择 WebSocket 预热。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FreeProbeMode {
    #[default]
    Http,
    WebsocketPrewarm,
}

/// 探测响应按事件交付，完整正文由临时文件端口承载，预览最多 64 KiB。
pub enum HttpProbeEvent {
    Headers(Box<HttpProbeExchange>),
    Progress {
        received_bytes: u64,
        preview: Vec<u8>,
    },
    Complete {
        elapsed_ms: u64,
        error: Option<String>,
    },
}

pub type HttpProbeEvents = futures::stream::BoxStream<'static, HttpProbeEvent>;

pub struct HttpProbeSession {
    pub events: HttpProbeEvents,
    pub body: std::sync::Arc<dyn crate::ports::proxy::HttpProbeBody>,
}

pub struct WebSocketPrewarmResult {
    pub exchange: HttpProbeExchange,
    pub body: Vec<u8>,
}

pub struct FreeProbeSession {
    pub id: String,
    pub events: HttpProbeEvents,
}

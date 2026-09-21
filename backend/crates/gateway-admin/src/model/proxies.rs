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
    pub response_body: Vec<u8>,
    pub elapsed_ms: u64,
    /// 读取失败仍返回已收到的报头和正文，并明确标明不完整。
    pub error: Option<String>,
}

pub struct FreeProbeCommand {
    pub account_id: Option<gateway_core::account::ProviderAccountId>,
    pub use_account_headers: bool,
    pub proxy: AccountProxySelection,
    pub request: HttpProbeRequest,
}

use async_trait::async_trait;
use gateway_core::account::{OutboundProxy, ProviderAccountId};

use super::store::AdminStoreResult;
use crate::model::{
    MutationContext, Revision,
    proxies::{
        ImportProxyBinding, NewProxy, ProxyAccountListQuery, ProxyAccountPage, ProxyListQuery,
        ProxyMutation, ProxyPage, ProxyRecord, ProxyTestResult, UpdateProxy,
    },
};

#[async_trait]
pub trait ProxyStore: Send + Sync {
    /// 在凭据交换到提交期间保护选定代理的连接配置和测试结果。
    async fn reserve_import(&self, id: &str) -> AdminStoreResult<ProxyImportReservation>;
    async fn list(&self, query: ProxyListQuery) -> AdminStoreResult<ProxyPage>;
    async fn list_accounts(
        &self,
        query: ProxyAccountListQuery,
    ) -> AdminStoreResult<ProxyAccountPage>;
    async fn get(&self, id: &str) -> AdminStoreResult<ProxyRecord>;
    /// 仅在账号仍绑定指定代理时解除关联，并清除账号保存的连接地址。
    async fn remove_account(
        &self,
        proxy_id: &str,
        account_id: &ProviderAccountId,
        context: &MutationContext,
    ) -> AdminStoreResult<Revision>;
    async fn create(
        &self,
        command: NewProxy,
        context: &MutationContext,
    ) -> AdminStoreResult<ProxyMutation>;
    async fn update(
        &self,
        command: UpdateProxy,
        context: &MutationContext,
    ) -> AdminStoreResult<ProxyMutation>;
    async fn delete(
        &self,
        id: &str,
        revision: Revision,
        context: &MutationContext,
    ) -> AdminStoreResult<Revision>;
    async fn record_test(
        &self,
        id: &str,
        revision: Revision,
        result: ProxyTestResult,
        context: &MutationContext,
    ) -> AdminStoreResult<ProxyRecord>;
}

/// 离开作用域时释放保护，错误返回和请求取消也遵循相同规则。
pub trait ProxyImportGuard: Send + Sync {}

pub struct ProxyImportReservation {
    pub binding: ImportProxyBinding,
    pub guard: Box<dyn ProxyImportGuard>,
}

#[async_trait]
pub trait ProxyProbe: Send + Sync {
    /// 显式管理员请求，不参与出口测试状态或账号调度。
    async fn send_http(
        &self,
        _proxy: Option<&OutboundProxy>,
        _request: crate::model::proxies::HttpProbeRequest,
    ) -> Result<crate::model::proxies::HttpProbeSession, crate::model::AdminError> {
        Err(crate::model::AdminError::invalid("当前实例不支持自由探测"))
    }

    async fn test(&self, proxy: &OutboundProxy) -> ProxyTestResult;
}

/// 临时正文只通过有界读取暴露，不将文件路径交给控制面或浏览器。
#[async_trait]
pub trait HttpProbeBody: Send + Sync {
    fn byte_length(&self) -> u64;
    fn finished_at(&self) -> Option<std::time::Instant>;
    async fn read(&self, offset: u64, length: usize) -> Result<Vec<u8>, crate::model::AdminError>;
}

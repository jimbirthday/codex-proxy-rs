//! State 探测策略与代理引用在同一设置行锁下提交，防止并发删除留下悬空引用。
use super::{
    AdminAuditEvent, PgControlPlaneRepository, append_admin_audit_event_in_transaction,
    bump_config_revision_in_transaction,
};
use crate::{Revision, StoreError, StoreResult, postgres_unavailable};
use gateway_admin::model::turn_state::TurnStateProbePolicy;
use sqlx::types::Json;

impl PgControlPlaneRepository {
    pub async fn load_turn_state_probe_policy(&self) -> StoreResult<TurnStateProbePolicy> {
        let Json(policy) = sqlx::query_scalar::<_, Json<TurnStateProbePolicy>>(
            "select turn_state_probe_policy_json from runtime_settings where id = 1",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|_| postgres_unavailable("load turn state probe policy"))?;
        policy.validate().map_err(|_| invalid_policy())?;
        Ok(policy)
    }

    pub async fn update_turn_state_probe_policy(
        &self,
        policy: TurnStateProbePolicy,
        audit: AdminAuditEvent,
    ) -> StoreResult<Revision> {
        policy.validate().map_err(|_| invalid_policy())?;
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|_| postgres_unavailable("begin turn state probe policy"))?;
        // 代理删除也先锁定此行；校验引用到提交期间目录不会被删除。
        let revision = bump_config_revision_in_transaction(&mut transaction).await?;
        let count = sqlx::query_scalar::<_, i64>(
            "select count(*) from outbound_proxies where id = any($1)",
        )
        .bind(&policy.proxy_ids)
        .fetch_one(&mut *transaction)
        .await
        .map_err(|_| postgres_unavailable("validate turn state proxy references"))?;
        if usize::try_from(count).ok() != Some(policy.proxy_ids.len()) {
            return Err(StoreError::InvalidData {
                entity: "turn state probe policy",
                message: "所选代理已不存在，请刷新后重新选择".to_owned(),
            });
        }
        sqlx::query("update runtime_settings set turn_state_probe_policy_json = $1 where id = 1")
            .bind(Json(policy))
            .execute(&mut *transaction)
            .await
            .map_err(|_| postgres_unavailable("save turn state probe policy"))?;
        append_admin_audit_event_in_transaction(&mut transaction, audit, revision).await?;
        transaction
            .commit()
            .await
            .map_err(|_| postgres_unavailable("commit turn state probe policy"))?;
        Ok(revision)
    }
}
fn invalid_policy() -> StoreError {
    StoreError::InvalidData {
        entity: "turn state probe policy",
        message: "状态探测策略不合法".to_owned(),
    }
}

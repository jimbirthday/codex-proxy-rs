## 立即验证实验

### 实验A：跨账号turn-state测试（最关键！）

```bash
# 步骤
1. 用"干净账号A"获取turn-state (假设得到292字符的)
2. 用"工作账号B"发请求，携带账号A的turn-state
3. 观察是否work，质量如何

# 预期结果
- 如果work且质量好 → 方案可行！
- 如果不work → turn-state可能绑定账号
- 如果work但质量差 → 还有其他限制因素
```

### 实验B：后端池质量对比

```bash
# 从你的数据库查询
1. 找出路由到unified-123的turn-state样本
2. 找出路由到unified-81的turn-state样本
3. 用相同prompt测试两者
4. 对比输出质量

# 测试prompt建议
"Implement a binary search tree in Rust with insert, delete, and search operations. Include comprehensive error handling and unit tests."
```

### 实验C：账号信誉分析

```bash
# 从响应头分析
x-codex-primary-used-percent: 70  (292字符)
x-codex-primary-used-percent: 31  (312字符)

# 假设
- 使用率低的账号 → 获得更好的turn-state？
- 使用率高的账号 → 被降级？

# 验证
统计你数据库中的样本，看used-percent与turn-state长度的相关性
```

---

## 数据库查询模板

```sql
-- 查询1：turn-state长度分布
SELECT
    response_turn_state_length,
    COUNT(*) as count
FROM turn_state_probe_exchanges
WHERE response_turn_state_length IS NOT NULL
GROUP BY response_turn_state_length
ORDER BY count DESC;

-- 查询2：同一账号的turn-state变化
SELECT
    account_id,
    response_turn_state_length,
    MIN(created_at) as first_seen,
    MAX(created_at) as last_seen,
    COUNT(*) as times
FROM turn_state_probe_exchanges
WHERE response_turn_state_length IS NOT NULL
GROUP BY account_id, response_turn_state_length
ORDER BY account_id, first_seen;

-- 查询3：turn-state的时效性
SELECT
    response_turn_state_sha256,
    MIN(created_at) as first_seen,
    MAX(created_at) as last_seen,
    COUNT(DISTINCT account_id) as used_by_accounts
FROM turn_state_probe_exchanges
WHERE response_turn_state_sha256 IS NOT NULL
GROUP BY response_turn_state_sha256
HAVING COUNT(*) > 1  -- 被多次观察到
ORDER BY used_by_accounts DESC;
```

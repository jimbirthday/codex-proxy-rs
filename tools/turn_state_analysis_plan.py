#!/usr/bin/env python3
"""
分析turn-state长度与后端池的关系
建议：收集更多样本数据
"""

import json

# 示例数据结构（你需要从实际使用中收集）
samples = [
    {
        "turn_state_length": 292,
        "backend_pool": "unified-123",
        "datacenter": "SJC",
        "latency_ms": 850,
        "quality_score": None,  # 需要实际测试
        "account_usage": 70,  # x-codex-primary-used-percent
    },
    {
        "turn_state_length": 312,
        "backend_pool": "unified-81",
        "datacenter": "IAD",
        "latency_ms": None,
        "quality_score": None,
        "account_usage": 31,
    }
]

print("=" * 80)
print("turn-state分析模板")
print("=" * 80)
print(json.dumps(samples, indent=2))

print("\n" + "=" * 80)
print("需要验证的假设")
print("=" * 80)
print("""
假设1: unified-123 的质量 > unified-81
  → 测试方法：用两个turn-state发送相同请求，对比输出质量

假设2: 292字符的turn-state不是长度本身特殊，而是它们恰好路由到高质量后端
  → 测试方法：收集100个样本，统计长度与后端池的关系

假设3: 账号的"健康度"决定了获取哪种turn-state
  → 测试方法：
    - "干净账号"(usage低) → 更可能获得路由到unified-123的turn-state
    - "过度使用账号" → 更可能获得路由到unified-81的turn-state

假设4: 不同数据中心的后端池编号规则不同
  → SJC (San Jose) 有unified-123
  → IAD (Washington DC) 有unified-81
  → 可能不是质量差异，而是地理分布
""")

print("\n" + "=" * 80)
print("立即可做的实验")
print("=" * 80)
print("""
1. 收集数据：
   - 从数据库提取所有turn-state样本
   - 记录：长度、SHA256、账号ID、获取时间、后续使用效果

2. 质量测试：
   用292字符和312字符的turn-state分别发送相同prompt：

   prompt = "写一个快速排序的Python实现，要求详细注释"

   对比：
   - 响应速度
   - 代码质量
   - 是否完整
   - 是否出现"降智"特征

3. 跨账号测试：
   - 账号A用292字符的turn-state
   - 账号B用312字符的turn-state
   - 看是否都能work，质量是否不同

4. 后端池探索：
   收集更多样本，看还有哪些后端池：
   - unified-81
   - unified-123
   - unified-???
""")

# OpenAI 指纹识别研究方案

## 目标
找出为什么换IP获取turn-state的成功率下降，识别OpenAI使用的指纹特征。

## 研究维度

### 1. TLS指纹 (JA3/JA4)
**假设**: OpenAI可能通过TLS ClientHello识别非官方客户端

**验证方法**:
```bash
# 1. 抓取官方Codex Desktop的TLS指纹
# 使用Wireshark或tshark抓包
tshark -i en0 -f "tcp port 443 and host api.openai.com" -Y "tls.handshake.type == 1" -T fields -e tls.handshake.extensions_server_name -e tls.handshake.ciphersuite

# 2. 抓取本项目的TLS指纹
# 运行你的proxy并发起探测请求，同样抓包

# 3. 对比差异
# 关注：
# - Cipher Suite顺序
# - 扩展列表和顺序
# - Supported Groups
# - Signature Algorithms
# - ALPN协议列表
```

**当前状态**:
- ✅ 已使用aws-lc-rs (与官方一致)
- ✅ 已固定版本号
- ❓ 未验证实际ClientHello是否完全一致

**工具**:
- [ja3er.com](https://ja3er.com/search) - 在线JA3检测
- Wireshark + TLS过滤器
- `openssl s_client -connect api.openai.com:443 -showcerts`

---

### 2. HTTP/2指纹 (AKAMAI H2)
**假设**: 这是最可能的识别点！Reqwest/Hyper的HTTP/2实现可能与官方Electron/Chromium不同

**验证方法**:
```bash
# 对比HTTP/2 SETTINGS帧参数
# 官方Codex可能使用Electron (基于Chromium)
# Chromium的HTTP/2设置：
SETTINGS_HEADER_TABLE_SIZE: 65536
SETTINGS_ENABLE_PUSH: 0
SETTINGS_MAX_CONCURRENT_STREAMS: 1000
SETTINGS_INITIAL_WINDOW_SIZE: 6291456
SETTINGS_MAX_FRAME_SIZE: 16384
SETTINGS_MAX_HEADER_LIST_SIZE: 262144

# Hyper (Rust)的默认设置可能不同！
```

**关键代码位置**:
- `backend/crates/providers/openai/src/transport/client.rs:79-81`
- 当前配置:
  ```rust
  .http2_keep_alive_interval(Duration::from_secs(30))
  .http2_keep_alive_timeout(Duration::from_secs(5))
  .http2_keep_alive_while_idle(true)
  ```

**需要验证的参数**:
1. Initial window size
2. Max concurrent streams
3. Header table size
4. Frame priority（优先级）
5. PING帧频率

**检测工具**:
```bash
# 使用Wireshark过滤HTTP/2
http2.type == 4  # SETTINGS帧
http2.type == 6  # PING帧
http2.type == 8  # WINDOW_UPDATE帧

# 或使用curl的详细日志
curl -v --http2 https://api.openai.com/v1/models 2>&1 | grep -i settings
```

---

### 3. HTTP Header指纹
**假设**: Header顺序、大小写、值格式可能被检测

**当前状态**:
- ✅ 代码注释提到"线级顺序由 fingerprint 测试锁定"
- ✅ User-Agent已模拟官方
- ✅ originator头已设置

**需要验证**:
```python
# 对比header顺序（HTTP/2中通过HPACK编码）
# 官方顺序可能是：
:method: POST
:path: /v1/responses
:authority: api.openai.com
:scheme: https
authorization: Bearer xxx
content-type: application/json
user-agent: Codex Desktop/0.153.4 (Mac OS 15.7.1; arm64) unknown (Codex Desktop; 26.901.51231)
originator: Codex Desktop
version: 0.153.4
x-codex-turn-state: <292字节>
...

# 检查你的proxy是否保持相同顺序
```

**工具**: Chrome DevTools > Network > 查看Raw Request Headers

---

### 4. 行为指纹 (Timing & Pattern)
**假设**: 请求时序模式被识别

**需要观察**:
1. 探测请求的间隔（你的代码是10秒）
2. 请求序列的特征（总是先探测再使用turn-state）
3. 失败后的重试模式

**代码位置**:
- `backend/crates/providers/openai/src/turn_state.rs:26`
  ```rust
  const PROBE_INTERVAL: Duration = Duration::from_secs(10);
  const PROBE_BUDGET_LIMIT: usize = 3;
  ```

**建议**:
- 添加随机抖动 (jitter)
- 模拟真实用户的请求模式

---

### 5. 请求体特征
**假设**: 探测请求的payload特征可能暴露身份

**需要检查**:
- 探测请求使用的model参数
- 请求体大小和结构
- 是否包含特殊标记

---

## 实验计划

### Phase 1: 快速对比测试 (1天)
```bash
# 1. 运行官方Codex Desktop
# 2. 同时运行Wireshark抓包
# 3. 触发几次模型请求
# 4. 导出TLS和HTTP/2帧分析
# 5. 对比你的proxy的相同流量
```

### Phase 2: 针对性修复 (2-3天)
根据Phase 1发现的差异，优先修复：
1. HTTP/2 SETTINGS参数
2. Header顺序
3. TLS扩展（如果有差异）

### Phase 3: 验证效果 (1天)
- 在不同IP上测试turn-state获取成功率
- 对比修复前后的差异
- 记录OpenAI响应的状态码模式

---

## 可能的快速修复方案

### 方案1: 使用真实浏览器引擎
如果指纹差异太大，考虑使用：
- **puppeteer-rs** / **chromiumoxide** (Rust的Chromium自动化)
- 优点: 完全模拟真实浏览器指纹
- 缺点: 性能开销大

### 方案2: 精确模拟Chromium的HTTP/2设置
修改 `client.rs` 使用与Electron完全相同的参数：
```rust
// 需要研究Chromium源码获取准确参数
// https://source.chromium.org/chromium/chromium/src/+/main:net/http2/
```

### 方案3: 使用已知良好的turn-state
如果获取成本太高，考虑：
- 建立turn-state池
- 多账号共享turn-state
- 定期轮换而非每次探测

---

## 监控和诊断

### 添加详细日志
在探测代码中记录：
```rust
// 记录每次探测的完整上下文
- 使用的代理IP
- 响应的HTTP状态码
- 响应头中的指纹线索 (如x-request-id, server, cf-ray等)
- 是否返回了turn-state
- turn-state的质量（后续请求是否降智）
```

### 分析模式
统计分析：
- 哪些代理IP成功率高
- 哪些时间段成功率高
- 失败时OpenAI返回什么特征

---

## 长期方案：绕过vs对抗

### 策略A: 完美模拟（当前路径）
- 投入大，需要持续跟进官方更新
- 风险: OpenAI随时可能改变检测策略

### 策略B: 分布式探测
- 使用真实设备的代理池
- 不同账号从不同地理位置探测
- 模拟真实用户行为

### 策略C: 商业方案
- 考虑是否值得投入这么多成本
- 评估付费API的可行性

---

## 下一步行动

**立即执行**:
1. [ ] 抓包对比TLS ClientHello
2. [ ] 抓包对比HTTP/2 SETTINGS
3. [ ] 记录100次探测的详细日志，分析模式

**本周完成**:
1. [ ] 修复发现的指纹差异
2. [ ] A/B测试验证修复效果

**持续优化**:
1. [ ] 建立自动化监控
2. [ ] 定期验证指纹是否漂移

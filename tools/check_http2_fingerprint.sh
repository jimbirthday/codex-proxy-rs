#!/bin/bash
# HTTP/2 指纹检测脚本
# 对比你的proxy和官方Codex的HTTP/2特征

echo "=== HTTP/2 指纹检测 ==="
echo ""

# 1. 检测你的proxy的HTTP/2指纹
echo "1. 检测本项目的HTTP/2指纹..."
echo "启动你的proxy，然后在另一个终端运行:"
echo "curl -v --http2 http://localhost:8080/v1/models -H 'Authorization: Bearer your-key' 2>&1 | grep -E '(SETTINGS|WINDOW_UPDATE|PING)'"
echo ""

# 2. 检测官方API的响应特征
echo "2. 直接请求OpenAI API (使用系统curl)..."
curl -v --http2 https://api.openai.com/v1/models \
  -H "Authorization: Bearer sk-test" \
  -H "User-Agent: Codex Desktop/0.153.4 (Mac OS 15.7.1; arm64) unknown (Codex Desktop; 26.901.51231)" \
  2>&1 | grep -i -E '(http/2|alpn|settings|server:|cf-ray:)'
echo ""

# 3. 使用openssl检查TLS
echo "3. 检查TLS指纹..."
echo "Q" | openssl s_client -connect api.openai.com:443 -alpn h2 2>&1 | grep -E '(Protocol|Cipher|ALPN)'
echo ""

echo "=== 建议 ==="
echo "1. 使用Wireshark抓包对比SETTINGS帧参数"
echo "2. 检查Header顺序是否一致"
echo "3. 验证ALPN协商结果"

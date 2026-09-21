#!/usr/bin/env python3
"""
解析 __oailb JWT cookie 获取后端路由信息
"""

import base64
import json
from getpass import getpass

def decode_jwt(token):
    """解码JWT（不验证签名）"""
    try:
        # JWT格式: header.payload.signature
        parts = token.split('.')
        if len(parts) != 3:
            print(f"错误：JWT格式不正确，部分数量={len(parts)}")
            return None

        # 解码payload（第二部分）
        payload = parts[1]
        # 补充padding
        padding = 4 - len(payload) % 4
        if padding != 4:
            payload += '=' * padding

        decoded = base64.urlsafe_b64decode(payload)
        data = json.loads(decoded)
        return data
    except Exception as e:
        print(f"解码错误: {e}")
        return None

# 运行时读取，避免将真实 Cookie 留在源码或 shell 历史中。
jwt1 = getpass("JWT #1: ")
jwt2 = getpass("JWT #2: ")

print("=" * 80)
print("响应1 (turn-state: 292字符)")
print("=" * 80)
data1 = decode_jwt(jwt1)
if data1:
    print(json.dumps(data1, indent=2))
    print(f"\n>>> 后端主机: {data1.get('host')}")

print("\n" + "=" * 80)
print("响应2 (turn-state: 312字符)")
print("=" * 80)
data2 = decode_jwt(jwt2)
if data2:
    print(json.dumps(data2, indent=2))
    print(f"\n>>> 后端主机: {data2.get('host')}")

print("\n" + "=" * 80)
print("对比")
print("=" * 80)
if data1 and data2:
    print(f"292字符 turn-state → {data1.get('host')}")
    print(f"312字符 turn-state → {data2.get('host')}")
    print(f"\n后端池编号: unified-123 vs unified-81")
    print(f"差异: {123 - 81} (后端池ID相差42)")

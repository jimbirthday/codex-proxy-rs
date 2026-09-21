#!/usr/bin/env python3
"""
分析 x-codex-turn-state 的结构
"""

import base64
import struct
from getpass import getpass
from datetime import datetime

def analyze_fernet_token(token):
    """分析Fernet格式的token"""
    try:
        # Base64解码
        decoded = base64.urlsafe_b64decode(token)

        print(f"原始长度: {len(token)} 字符")
        print(f"解码后长度: {len(decoded)} 字节")
        print(f"十六进制前16字节: {decoded[:16].hex()}")

        # Fernet格式解析
        if decoded[0] == 0x80:
            print("版本字节为 0x80，与 Fernet 格式相符（未验证签名）")

        # 提取时间戳（字节1-8）
        timestamp_bytes = decoded[1:9]
        timestamp = struct.unpack('>Q', timestamp_bytes)[0]
        dt = datetime.fromtimestamp(timestamp)
        print(f"时间戳: {timestamp}")
        print(f"时间: {dt}")

        # IV（字节9-24）
        iv = decoded[9:25]
        print(f"IV (前8字节): {iv[:8].hex()}")

        # 剩余是密文+HMAC
        remaining = decoded[25:]
        print(f"密文+HMAC长度: {len(remaining)} 字节")

        if len(remaining) >= 32:
            hmac = remaining[-32:]
            ciphertext = remaining[:-32]
            print(f"密文长度: {len(ciphertext)} 字节")
            print(f"HMAC (前8字节): {hmac[:8].hex()}")

        return {
            'length': len(decoded),
            'timestamp': timestamp,
            'datetime': dt,
            'iv': iv.hex(),
            'ciphertext_length': len(ciphertext) if 'ciphertext' in locals() else 0
        }

    except Exception as e:
        print(f"解析错误: {e}")
        return None

# 运行时读取，避免将真实 State 留在源码或 shell 历史中。
state1 = getpass("turn-state #1: ")
state2 = getpass("turn-state #2: ")

print("=" * 70)
print("分析 turn-state #1 (292字节)")
print("=" * 70)
info1 = analyze_fernet_token(state1)

print("\n" + "=" * 70)
print("分析 turn-state #2 (非292字节)")
print("=" * 70)
info2 = analyze_fernet_token(state2)

print("\n" + "=" * 70)
print("对比分析")
print("=" * 70)
if info1 and info2:
    print(f"长度差异: {info1['length']} vs {info2['length']} ({info1['length'] - info2['length']} 字节)")
    print(f"时间差: {abs(info1['timestamp'] - info2['timestamp'])} 秒")
    print(f"密文长度差异: {info1.get('ciphertext_length', 0)} vs {info2.get('ciphertext_length', 0)}")

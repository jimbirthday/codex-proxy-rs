#!/usr/bin/env python3
"""逐条检查 State 长度，不保存或回显原始值。"""

import base64
import binascii
import sys
from getpass import getpass


def show_length(value):
    # 粘贴产生的首尾空白不计入报头值，正文中的空白仍保留。
    value = value.strip()
    if not value:
        return False

    print(f"字符数：{len(value)}")
    print(f"UTF-8 字节数：{len(value.encode('utf-8'))}")
    try:
        decoded = base64.b64decode(value, altchars=b"-_", validate=True)
        print(f"Base64 解码后：{len(decoded)} 字节")
    except (ValueError, binascii.Error):
        print("Base64 解码失败：格式或填充不合法")
    print()
    return True


def main():
    if not sys.stdin.isatty():
        for line in sys.stdin:
            show_length(line)
        return

    print("粘贴 State 后按回车；输入不回显，直接回车退出")
    try:
        while show_length(getpass("State: ")):
            pass
    except (EOFError, KeyboardInterrupt):
        print()


if __name__ == "__main__":
    main()

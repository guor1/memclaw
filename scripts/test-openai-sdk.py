#!/usr/bin/env python3
"""TC-H12: 用真实 OpenAI Python SDK 指向本网关跑一次对话。

验证标准客户端能解析我们的非流式响应与 SSE 流。
用法: python scripts/test-openai-sdk.py [base_url]
"""
import sys
import time

from openai import OpenAI

BASE = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:8931/v1"

client = OpenAI(
    base_url=BASE,
    api_key="local",  # 网关无鉴权，SDK 要求非空即可
    timeout=120.0,
)


def report(name, ok, detail=""):
    mark = "PASS" if ok else "FAIL"
    print(f"[{mark}] {name}" + (f"  {detail}" if detail else ""))
    return ok


def main():
    ok = True

    # 1) 非流式：一次完整对话。
    t0 = time.time()
    try:
        resp = client.responses.create(
            model="doubao-seed-2-1-pro-260628",
            input="用一句话回答：1+1 等于几？",
        )
        text = resp.output_text
        print(f"  非流式回复: {text!r}  耗时 {time.time()-t0:.1f}s")
        ok &= report("非流式响应解析", bool(text), f"output_text={text!r}")
    except Exception as e:
        ok &= report("非流式响应解析", False, f"{type(e).__name__}: {e}")

    # 2) 流式：SSE 应被 SDK 正确解析成增量文本。
    t0 = time.time()
    try:
        chunks = []
        with client.responses.stream(
            model="doubao-seed-2-1-pro-260628",
            input="从 1 数到 5，每步一行。",
        ) as stream:
            for event in stream:
                # 只收文本增量；其它事件（created/completed）也应能正常迭代。
                dt = getattr(event, "type", None)
                if dt == "response.output_text.delta":
                    chunks.append(getattr(event, "delta", ""))
        text = "".join(chunks)
        print(f"  流式文本: {text!r}  耗时 {time.time()-t0:.1f}s")
        ok &= report("SSE 流式解析", bool(text), f"delta 累计 {len(chunks)} 段")
    except Exception as e:
        ok &= report("SSE 流式解析", False, f"{type(e).__name__}: {e}")

    print("\n结果:", "全部通过" if ok else "存在失败")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()

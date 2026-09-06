#!/usr/bin/env bash
# TC-H3 / TC-H4 / TC-H8 用的并发打点脚本。
#
# 为什么要它：直接 `curl ... &` 虽然并发了，但输出看不出「真的重叠」——
# 顺序跑完也是同样的输出。这里给每个请求记开始/结束的毫秒时间戳，
# 用重叠区间证明并发确实发生了。
#
# 用法：
#   bash scripts/h3-concurrent.sh [并发数] [session-key] [提示词]
#   bash scripts/h3-concurrent.sh 3  race-test "只回答这个数字本身："
#   bash scripts/h3-concurrent.sh 20 queue-full "分 10 条详述 Rust 所有权"
#
# 环境变量：
#   PORT     网关端口，默认 8080
#   DISTINCT 1=每个请求追加自己的序号（验内容不串台），0=所有请求同一提示词
set -u

# Windows 上 Python stdout 默认 GBK，打印非 GBK 字符（含 ✅ ⚠ █）会 UnicodeEncodeError。
export PYTHONIOENCODING=utf-8

N="${1:-3}"
KEY="${2:-race-test}"
PROMPT="${3:-只回答这个数字本身：}"
PORT="${PORT:-8080}"
DISTINCT="${DISTINCT:-1}"

OUT="$(mktemp -d)"
echo "并发数=$N  session-key=$KEY  端口=$PORT  输出目录=$OUT"
echo

now_ms() { python -c 'import time; print(int(time.time()*1000))'; }

T0=$(now_ms)

for i in $(seq 1 "$N"); do
  (
    if [ "$DISTINCT" = "1" ]; then
      body=$(python -c '
import json,sys
print(json.dumps({"model":"default","input":sys.argv[1]+sys.argv[2]}))' "$PROMPT" "$i")
    else
      body=$(python -c '
import json,sys
print(json.dumps({"model":"default","input":sys.argv[1]}))' "$PROMPT")
    fi

    s=$(now_ms)
    code=$(curl -s -o "$OUT/resp-$i.json" -w '%{http_code}' \
      --max-time 180 \
      "localhost:$PORT/v1/responses" \
      -H 'content-type: application/json' \
      -H "x-openclaw-session-key: $KEY" \
      -d "$body")
    e=$(now_ms)
    echo "$i $code $((s - T0)) $((e - T0))" > "$OUT/time-$i.txt"
  ) &
done

echo "已发出 $N 个并发请求，等待全部返回…（每个最多 180s）"
wait
echo

python - "$OUT" "$T0" <<'PY'
import json, os, sys, glob

out, t0 = sys.argv[1], int(sys.argv[2])
rows = []
for f in sorted(glob.glob(os.path.join(out, "time-*.txt")),
                key=lambda p: int(p.rsplit("-", 1)[1].split(".")[0])):
    i, code, s, e = open(f).read().split()
    rows.append((int(i), code, int(s), int(e)))

if not rows:
    print("没有任何请求完成——检查网关是否在跑")
    raise SystemExit(1)

span = max(r[3] for r in rows) or 1
W = 44
print("时间轴（█ = 请求在飞，证明重叠即证明并发）")
print(f"  {'#':>3} {'码':>4} {'起':>6} {'止':>6} {'耗时':>6}  0{'─' * (W - 2)}{span}ms")
for i, code, s, e in rows:
    a = int(s / span * W)
    b = max(int(e / span * W), a + 1)
    bar = " " * a + "█" * (b - a)
    print(f"  {i:>3} {code:>4} {s:>6} {e:>6} {e - s:>6}  {bar}")

# 重叠判定：任意两个区间相交即为真并发
overlap = max(
    (sum(1 for _, _, s2, e2 in rows if s2 < e and e2 > s) for _, _, s, e in rows),
    default=0,
)
print(f"\n最大同时在飞数：{overlap} / {len(rows)}")
if overlap <= 1:
    print("  ⚠️ 没有重叠——请求是顺序执行的！并发没生效。")
    print("     最常见原因：在 cmd.exe 里跑（& 是命令分隔符，不是后台符）。")
else:
    print("  ✅ 有重叠，确认真并发。")

print("\n返回结果：")
ok = bad = 0
for i, code, s, e in rows:
    p = os.path.join(out, f"resp-{i}.json")
    txt, usage, status = "(空)", "?", "?"
    try:
        d = json.load(open(p, encoding="utf-8"))
        status = d.get("status", "?")
        usage = (d.get("usage") or {}).get("input_tokens", "?")
        for it in d.get("output") or []:
            for c in it.get("content") or []:
                if c.get("text"):
                    txt = c["text"].replace("\n", " ")[:60]
                    break
        if txt == "(空)":
            txt = json.dumps(d, ensure_ascii=False)[:80]
    except Exception as ex:
        txt = f"(解析失败: {ex})"
    flag = "ok " if code == "200" else "ERR"
    ok += code == "200"
    bad += code != "200"
    print(f"  [{flag}] #{i} http={code} status={status} in_tok={usage}  {txt}")

print(f"\n汇总：{ok} 成功 / {bad} 失败 / 共 {len(rows)}")
if bad == 0 and len(rows) == int(os.environ.get("EXPECT_N", len(rows))):
    print("无请求挂死（全部在超时内返回）")
print(f"\n原始响应留在：{out}")
PY

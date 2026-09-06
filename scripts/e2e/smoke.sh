#!/usr/bin/env bash
# 进程级冒烟：验证 `cargo test` 覆盖不到的那一层。
#
# cargo test 里的 e2e 直接调 serve_with()/create_app()，因此碰不到：
#   clap 参数解析、config_loader、paths.rs、SecretRef 解引用、真二进制启动、
#   单实例锁、provider 的 HTTP/SSE 实际解析路径。
# 历史上 `OC_HOME` 写成 MSYS 路径导致数据静默落到垃圾目录，就只在这一层暴露。
#
# 用法：
#   bash scripts/e2e/smoke.sh
#   OC_BIN=target/release/oc bash scripts/e2e/smoke.sh
#
# 退出码非 0 即失败；CI 直接据此判定。
set -euo pipefail

# ── 前置自检 ────────────────────────────────────────────────────
# 必须是 bash：下面用了后台任务、$(...)、[[ ]]。在 cmd.exe 里 `&` 是命令分隔符，
# 会把并发变成顺序执行——这类失败是**静默**的，看着像通过。
[ -n "${BASH_VERSION:-}" ] || { echo "必须用 bash 跑本脚本"; exit 1; }
command -v curl >/dev/null || { echo "缺 curl"; exit 1; }

OC_BIN="${OC_BIN:-target/debug/oc}"
[ -x "$OC_BIN" ] || OC_BIN="${OC_BIN}.exe"
[ -x "$OC_BIN" ] || { echo "找不到可执行文件：${OC_BIN}（先 cargo build --bin oc）"; exit 1; }
OC_BIN="$(cd "$(dirname "$OC_BIN")" && pwd)/$(basename "$OC_BIN")"

PASS=0; FAIL=0
ok()   { echo "  [ok ] $1"; PASS=$((PASS+1)); }
bad()  { echo "  [FAIL] $1"; FAIL=$((FAIL+1)); }
check(){ if eval "$2" >/dev/null 2>&1; then ok "$1"; else bad "$1"; fi; }

# ── 隔离的 OC_HOME ──────────────────────────────────────────────
# ⚠️ Windows 上必须用 Windows 路径格式（C:\... 或 C:/...）。写成 MSYS 风格
# /c/... 会被 Rust 当成当前盘根下的 \c\...，数据落到垃圾路径，而 `oc doctor`
# 原样回显、库照样建得出来，**完全看不出错**。
WORK="$(mktemp -d)"
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) OC_HOME="$(cygpath -w "$WORK")" ;;
  *)                    OC_HOME="$WORK" ;;
esac
export OC_HOME
echo "OC_HOME=$OC_HOME"
echo "OC_BIN=$OC_BIN"
echo

# ⚠️ Windows 上 OC_HOME 只决定库/配置位置，**不隔离连接**：管道名是全局常量
# \\.\pipe\oc-daemon（TransportKind::platform_default 直接忽略 oc_home）。
# 故用 OC_SOCKET 覆盖端点——`oc serve` 与所有 CLI/TUI 客户端都尊重它。
# 不这么做的话，冒烟会连上开发机正跑着的那个 daemon：读到别人的数据、
# 却一路 ok，是最典型的假通过。
if [[ "$(uname -s)" == MINGW* || "$(uname -s)" == MSYS* || "$(uname -s)" == CYGWIN* ]]; then
  OC_SOCKET="\\\\.\\pipe\\oc-smoke-$$"
else
  OC_SOCKET="$WORK/oc.sock"
fi
export OC_SOCKET
echo "OC_SOCKET=$OC_SOCKET"

PORT="${PORT:-18789}"
SERVE_PID=""; HTTP_PID=""; MOCK_PID=""
cleanup() {
  for p in $HTTP_PID $SERVE_PID $MOCK_PID; do
    kill "$p" 2>/dev/null || true
  done
  wait 2>/dev/null || true
  rm -rf "$WORK" 2>/dev/null || true
}
trap cleanup EXIT

# ── 0. mock 模型端点 ────────────────────────────────────────────
# 让冒烟不需要 API key，且真的走到 openai.rs / sse.rs 的解析路径
# （config 的 base_url 指过来）。
MOCK_PORT="${MOCK_PORT:-18790}"
python - "$MOCK_PORT" <<'PY' &
import sys, json
from http.server import BaseHTTPRequestHandler, HTTPServer

class H(BaseHTTPRequestHandler):
    def do_POST(self):
        n = int(self.headers.get("content-length", 0))
        self.rfile.read(n)
        # OpenAI 兼容的 SSE 流式应答，最小可用形态。
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.end_headers()
        for chunk in ["SMOKE", "_OK"]:
            d = {"choices": [{"delta": {"content": chunk}, "finish_reason": None}]}
            self.wfile.write(f"data: {json.dumps(d)}\n\n".encode())
        d = {"choices": [{"delta": {}, "finish_reason": "stop"}]}
        self.wfile.write(f"data: {json.dumps(d)}\n\n".encode())
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()
    def log_message(self, *a): pass

HTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
PY
MOCK_PID=$!

# 等 mock 真的在听。不等就往下走的话，daemon 第一次调模型会连接失败，
# 而失败表现是「/v1/responses 不返回 completed」——排查方向会被带偏到网关上。
mready=0
for _ in $(seq 1 30); do
  if curl -s --max-time 2 -X POST "localhost:${MOCK_PORT}/chat/completions" \
       -H 'content-type: application/json' -d '{}' >/dev/null 2>&1; then
    mready=1; break
  fi
  sleep 0.2
done
[ "$mready" = 1 ] || { echo "mock 模型端点起不来（端口 ${MOCK_PORT} 可能被占）"; exit 1; }

# ── 1. 配置 ─────────────────────────────────────────────────────
echo "1. 配置与 doctor"
mkdir -p "$WORK"
# 以 config.example.toml 为基准，只改冒烟需要的几项。
# 不手写整份配置：那样每次给 Config 加必填字段都会让本脚本失效，
# 而失效方式是「daemon 起不来」而非「配置示例过期」，排查方向会被带偏。
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
python - "$REPO_ROOT/config.example.toml" "$WORK/config.toml" "$MOCK_PORT" "$(uname -s)" <<'PY'
import re, sys
src, dst, mock_port, uname = sys.argv[1:5]
cfg = open(src, encoding="utf-8").read()

windows = uname.startswith(("MINGW", "MSYS", "CYGWIN"))
# 整行替换（含行尾注释）——用 '.*"' 会贪婪吃掉注释里的引号，产出非法 TOML。
def set_line(text, key, value):
    return re.sub(r'^%s\s*=.*$' % re.escape(key), '%s = %s' % (key, value),
                  text, count=1, flags=re.M)

cfg = set_line(cfg, "transport", '"%s"' % ("pipe" if windows else "unix"))
# 模型指向本地 mock 端点：不需要 API key，且真的走 openai.rs / sse.rs 解析路径。
cfg = set_line(cfg, "base_url", '"http://127.0.0.1:%s"' % mock_port)
cfg = set_line(cfg, "model", '"smoke-model"')
cfg = set_line(cfg, "api_key", '{ inline = "smoke-key" }')
# 冒烟不做交互，审批一律拒绝，免得卡在等回执。
cfg = set_line(cfg, "mode", '"deny"')
open(dst, "w", encoding="utf-8", newline="\n").write(cfg)
PY

# doctor：建库 + 校验配置。这一步同时验证 config_loader 与 paths.rs 真的用了 OC_HOME。
check "oc doctor 通过" "'$OC_BIN' doctor"
# 反向验证 OC_HOME 真的生效了——库必须落在隔离目录里，而不是 ~/.oc。
check "库落在隔离的 OC_HOME 内" "[ -f '$WORK/oc.sqlite' ]"

# ── 2. daemon ───────────────────────────────────────────────────
echo
echo "2. daemon 启动"
# 开 debug 日志：失败时下面会 tail 出来，模型调用失败的真因（连不上 mock、
# SSE 解析报错等）只在这个级别可见。
RUST_LOG="${RUST_LOG:-oc=debug,oc_server=debug,oc_llm=debug}" \
  "$OC_BIN" serve > "$WORK/serve.log" 2>&1 &
SERVE_PID=$!

ready=0
for _ in $(seq 1 50); do
  if "$OC_BIN" sessions >/dev/null 2>&1; then ready=1; break; fi
  sleep 0.2
done
[ "$ready" = 1 ] && ok "daemon 就绪" || { bad "daemon 未就绪"; sed -n '1,40p' "$WORK/serve.log"; }

# ── 3. CLI 往返（原手册 TC-P1-2a）────────────────────────────────
echo
echo "3. CLI 往返"
check "sessions 可用"              "'$OC_BIN' sessions"
check "status 可用"                "'$OC_BIN' status"
check "debug 可用"                 "'$OC_BIN' debug"

CRON_OUT="$("$OC_BIN" cron add '0 9 * * 1-5' '冒烟提醒' 2>&1 || true)"
check "cron add 后能在 list 里看到"  "'$OC_BIN' cron list | grep -q 冒烟提醒"
CRON_ID="$("$OC_BIN" cron list 2>/dev/null | grep -o 'cron-[A-Za-z0-9_-]*' | head -1 || true)"
if [ -n "$CRON_ID" ]; then
  check "cron rm 后从 list 消失"     "'$OC_BIN' cron rm '$CRON_ID' && ! '$OC_BIN' cron list | grep -q 冒烟提醒"
else
  bad "取不到 cron id（cron list 输出：$("$OC_BIN" cron list 2>&1 | head -3)）"
fi

check "intent add 后能在 list 里看到" "'$OC_BIN' intent add 带转换插头 出差 && '$OC_BIN' intent list | grep -q 带转换插头"
INTENT_ID="$("$OC_BIN" intent list 2>/dev/null | grep -o 'intent-[A-Za-z0-9-]*' | head -1 || true)"
if [ -n "$INTENT_ID" ]; then
  check "intent rm 后从 list 消失"   "'$OC_BIN' intent rm '$INTENT_ID' && ! '$OC_BIN' intent list | grep -q 带转换插头"
else
  bad "取不到 intent id（intent list 输出：$("$OC_BIN" intent list 2>&1 | head -3)）"
fi

# ── 4. HTTP 网关 ────────────────────────────────────────────────
echo
echo "4. HTTP 网关"
"$OC_BIN" http --port "$PORT" > "$WORK/http.log" 2>&1 &
HTTP_PID=$!

hready=0
for _ in $(seq 1 50); do
  if curl -sf "localhost:$PORT/health" >/dev/null 2>&1; then hready=1; break; fi
  sleep 0.2
done
[ "$hready" = 1 ] && ok "/health 就绪" || { bad "网关未就绪"; sed -n '1,40p' "$WORK/http.log"; }

if [ "$hready" = 1 ]; then
  # 打通一次真实请求：HTTP → daemon → provider(mock 端点) → SSE 解析 → 回执。
  RESP="$(curl -s --max-time 30 "localhost:$PORT/v1/responses" \
      -H 'content-type: application/json' \
      -d '{"model":"default","input":"ping"}' || true)"
  check "/v1/responses 返回 completed"  "echo '$RESP' | grep -q completed"
  check "回复文本经 provider 链路传回（SMOKE_OK）"    "echo '$RESP' | grep -q SMOKE_OK"
  echo "$RESP" | grep -q completed || echo "      实际响应：$(echo "$RESP" | head -c 400)"

  # 保留命名空间的 session key 应 400（原手册 TC-H10）。
  #
  # 注意用 `cron:` 前缀而非「看起来很怪」的 key：validate_session_key 只拒
  # 空串 / 超长 / 控制符 / 保留前缀，`bad key!!` 这种带空格叹号的其实**合法**。
  # 最初这条用的就是后者，它返回的 400 实际来自 JSON body 解析失败——
  # 断言通过了但测的完全是另一件事，正是要防的那种假通过。
  CODE="$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 \
      "localhost:$PORT/v1/responses" \
      -H 'content-type: application/json' \
      -H 'x-openclaw-session-key: cron:smoke' \
      -d '{"model":"default","input":"x"}' || true)"
  [ "$CODE" = "400" ] && ok "保留前缀 session key 返回 400" || bad "保留前缀 session key 返回 $CODE（期望 400）"

  # 合法但含空格的 key 应被接受（与上一条互为对照，证明 400 来自前缀判定
  # 而非「请求随便就会 400」）。
  CODE2="$(curl -s -o /dev/null -w '%{http_code}' --max-time 30 \
      "localhost:$PORT/v1/responses" \
      -H 'content-type: application/json' \
      -H 'x-openclaw-session-key: smoke lane 1' \
      -d '{"model":"default","input":"x"}' || true)"
  [ "$CODE2" = "200" ] && ok "含空格的合法 key 被接受（200）" || bad "含空格的合法 key 返回 $CODE2（期望 200）"

  # 裸请求应落 main，不该冒出 http-<uuid> 会话（原手册 TC-H1）。
  check "裸请求落 main，无 http-* 会话" "! '$OC_BIN' sessions | grep -q 'http-'"
fi

# ── 汇总 ────────────────────────────────────────────────────────
echo
echo "────────────────────────────"
echo "通过 $PASS / 失败 $FAIL"
[ "$FAIL" -eq 0 ] || { echo; echo "--- serve.log ---"; tail -30 "$WORK/serve.log" 2>/dev/null; echo "--- http.log ---"; tail -30 "$WORK/http.log" 2>/dev/null; exit 1; }
echo "冒烟通过"

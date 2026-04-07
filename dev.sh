#!/bin/bash
# dev.sh — ClewdR 开发调试一键脚本
# 用法: ./dev.sh [选项]
#   无参数:                重启后端（前端复用已构建的 static/）
#   rebuild:               先构建前端 static/ 再启动后端
#   reset:                 删除 DB 重新初始化（默认凭据 admin:password）
#   hmr:                   启动后端 + Vite dev server（全栈开发）
#   rebuild hmr:           重建 static/，再启动后端 + Vite HMR
#   reset hmr:             重置 DB，再启动后端 + Vite HMR
#   stop:                  停止 dev.sh 启动的后端/Vite 进程
#   no-timeout:            关闭自动停机 watchdog
#   timeout=SECONDS:       覆盖自动停机时长（默认 10800 秒 = 3 小时）

set -euo pipefail
cd "$(dirname "$0")"
git config core.hooksPath .githooks 2>/dev/null || true

DB_FILE="clewdr.db"
BIND_IP="${CLEWDR_IP:-0.0.0.0}"
BIND_PORT="${CLEWDR_PORT:-8484}"
VITE_HOST="${VITE_DEV_HOST:-0.0.0.0}"
VITE_PORT="${VITE_DEV_PORT:-3000}"
VITE_BACKEND_URL="${VITE_DEV_BACKEND_URL:-http://localhost:${BIND_PORT}}"

BACKEND_PID_FILE=".dev-backend.pid"
FRONTEND_PID_FILE=".dev-frontend.pid"
WATCHDOG_PID_FILE=".dev-watchdog.pid"
WATCHDOG_TOKEN_FILE=".dev-watchdog.token"
BACKEND_LOG_FILE=".dev-backend.log"
FRONTEND_LOG_FILE=".dev-frontend.log"
WATCHDOG_LOG_FILE=".dev-watchdog.log"

DO_REBUILD=false
DO_RESET=false
DO_HMR=false
DO_STOP=false
AUTO_STOP_SECONDS="${DEV_AUTO_STOP_SECONDS:-10800}"
DISABLE_TIMEOUT=false

for arg in "$@"; do
  case "$arg" in
    rebuild) DO_REBUILD=true ;;
    reset)   DO_RESET=true ;;
    hmr)     DO_HMR=true ;;
    stop)    DO_STOP=true ;;
    no-timeout) DISABLE_TIMEOUT=true ;;
    timeout=*)
      AUTO_STOP_SECONDS="${arg#timeout=}"
      ;;
    *)       echo "未知参数: $arg"; exit 1 ;;
  esac
done

stop_pid_file() {
  local pid_file="$1"
  if [ ! -f "$pid_file" ]; then
    return
  fi
  local pid
  pid="$(cat "$pid_file" 2>/dev/null || true)"
  if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
    kill "$pid" 2>/dev/null || true
    sleep 1
    kill -9 "$pid" 2>/dev/null || true
  fi
  rm -f "$pid_file"
}

wait_http() {
  local url="$1"
  local label="$2"
  local timeout="${3:-60}"

  for _ in $(seq 1 "$timeout"); do
    if curl -s -o /dev/null -w "" "$url" 2>/dev/null; then
      echo "==> ${label}已就绪: ${url}"
      return 0
    fi
    sleep 1
  done

  echo "==> ${label}启动超时，最近日志："
  return 1
}

is_positive_int() {
  local value="$1"
  [[ "$value" =~ ^[0-9]+$ ]] && [ "$value" -gt 0 ]
}

start_watchdog() {
  local timeout="$1"
  if ! is_positive_int "$timeout"; then
    rm -f "$WATCHDOG_PID_FILE" "$WATCHDOG_TOKEN_FILE"
    echo "==> 自动停机已关闭"
    return 0
  fi

  local expected_token
  expected_token="$(date +%s)-$$"
  echo "$expected_token" > "$WATCHDOG_TOKEN_FILE"

  nohup bash -c "
    date -Is
    echo 'watchdog started: timeout=${timeout}s token=${expected_token}'
    sleep $timeout
    cd '$PWD'
    current_token=\$(cat '$WATCHDOG_TOKEN_FILE' 2>/dev/null || true)
    if [ \"\$current_token\" = '$expected_token' ]; then
      date -Is
      echo 'watchdog timeout reached, stopping dev stack'
      ./dev.sh stop || true
    else
      date -Is
      echo 'watchdog token changed, skip stop'
      echo \"current_token=\$current_token\"
    fi
    current_token=\$(cat '$WATCHDOG_TOKEN_FILE' 2>/dev/null || true)
    if [ \"\$current_token\" = '$expected_token' ]; then
      rm -f '$WATCHDOG_PID_FILE' '$WATCHDOG_TOKEN_FILE'
    fi
  " >"$WATCHDOG_LOG_FILE" 2>&1 &
  local watchdog_pid=$!
  echo "$watchdog_pid" > "$WATCHDOG_PID_FILE"
  echo "==> 已启用自动停机: ${timeout}s 后执行 ./dev.sh stop (PID: ${watchdog_pid})"
}

stop_pid_file "$BACKEND_PID_FILE"
stop_pid_file "$FRONTEND_PID_FILE"
stop_pid_file "$WATCHDOG_PID_FILE"
rm -f "$WATCHDOG_TOKEN_FILE"

pkill -9 -f "target/debug/clewdr --db ${DB_FILE}" 2>/dev/null || true
pkill -9 -f "target/release/clewdr --db ${DB_FILE}" 2>/dev/null || true
pkill -9 -f "npm --prefix frontend run dev" 2>/dev/null || true
pkill -9 -f "vite --host ${VITE_HOST} --port ${VITE_PORT}" 2>/dev/null || true
sleep 1

if $DO_STOP; then
  rm -f "$WATCHDOG_TOKEN_FILE"
  echo "==> 已停止 dev 后端/Vite 进程"
  exit 0
fi

if $DISABLE_TIMEOUT; then
  AUTO_STOP_SECONDS=0
fi

if [ "$AUTO_STOP_SECONDS" != "0" ] && ! is_positive_int "$AUTO_STOP_SECONDS"; then
  echo "自动停机参数无效: $AUTO_STOP_SECONDS（应为正整数秒，或使用 no-timeout / DEV_AUTO_STOP_SECONDS=0 关闭）"
  exit 1
fi

if $DO_RESET; then
  echo "==> 删除 $DB_FILE（重新初始化）..."
  rm -f "$DB_FILE" "${DB_FILE}-wal" "${DB_FILE}-shm"
  if [ -f "$DB_FILE" ]; then
    echo "==> 警告: $DB_FILE 仍存在，可能有进程占用"
    fuser -k "$DB_FILE" 2>/dev/null || true
    sleep 1
    rm -f "$DB_FILE" "${DB_FILE}-wal" "${DB_FILE}-shm"
  fi
fi

if $DO_REBUILD; then
  echo "==> 构建前端 static/..."
  npm --prefix frontend run build
fi

if ! $DO_HMR && [ ! -f static/index.html ]; then
  echo "==> static/index.html 不存在，先构建前端..."
  npm --prefix frontend run build
fi

echo "==> 编译 + 启动后端..."
nohup env CLEWDR_IP="$BIND_IP" CLEWDR_PORT="$BIND_PORT" \
  cargo run -- --db "$DB_FILE" >"$BACKEND_LOG_FILE" 2>&1 &
BACKEND_PID=$!
echo "$BACKEND_PID" > "$BACKEND_PID_FILE"

echo "==> 等待后端启动 (PID: $BACKEND_PID)..."
if ! wait_http "http://localhost:${BIND_PORT}/api/version" "后端" 60; then
  tail -n 60 "$BACKEND_LOG_FILE" || true
  exit 1
fi

if $DO_HMR; then
  echo "==> 启动 Vite dev server (PID file: $FRONTEND_PID_FILE)..."
  nohup env VITE_DEV_BACKEND_URL="$VITE_BACKEND_URL" VITE_DEV_HOST="$VITE_HOST" VITE_DEV_PORT="$VITE_PORT" \
    npm --prefix frontend run dev -- --host "$VITE_HOST" --port "$VITE_PORT" >"$FRONTEND_LOG_FILE" 2>&1 &
  FRONTEND_PID=$!
  echo "$FRONTEND_PID" > "$FRONTEND_PID_FILE"

  echo "==> 等待前端启动 (PID: $FRONTEND_PID)..."
  if ! wait_http "http://localhost:${VITE_PORT}" "前端" 60; then
    tail -n 60 "$FRONTEND_LOG_FILE" || true
    exit 1
  fi

  start_watchdog "$AUTO_STOP_SECONDS"

  cat <<EOF
==> 全栈开发环境已启动
后端 API: http://localhost:${BIND_PORT}
前端 HMR: http://localhost:${VITE_PORT}
后端日志: ${BACKEND_LOG_FILE}
前端日志: ${FRONTEND_LOG_FILE}
watchdog日志: ${WATCHDOG_LOG_FILE}
停止命令: ./dev.sh stop
EOF
  exit 0
fi

start_watchdog "$AUTO_STOP_SECONDS"

cat <<EOF
==> 后端开发环境已启动
访问地址: http://localhost:${BIND_PORT}
后端日志: ${BACKEND_LOG_FILE}
watchdog日志: ${WATCHDOG_LOG_FILE}
停止命令: ./dev.sh stop
EOF

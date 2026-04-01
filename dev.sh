#!/bin/bash
# dev.sh — ClewdR 开发调试一键脚本
# 用法: ./dev.sh [选项]
#   无参数:     仅重启后端（前端用已构建的 static/）
#   rebuild:    先构建前端再启动后端
#   reset:      删除 DB 重新初始化（默认凭据 admin:password）
#   rebuild reset: 两者都做

set -e
cd "$(dirname "$0")"

DB_FILE="clewdr.db"
BIND_IP="${CLEWDR_IP:-0.0.0.0}"
BIND_PORT="${CLEWDR_PORT:-8484}"

# 解析参数
DO_REBUILD=false
DO_RESET=false
for arg in "$@"; do
  case "$arg" in
    rebuild) DO_REBUILD=true ;;
    reset)   DO_RESET=true ;;
    *)       echo "未知参数: $arg"; exit 1 ;;
  esac
done

# 1. 杀旧进程
pkill -9 -f "target/debug/clewdr" 2>/dev/null || true
pkill -9 -f "target/release/clewdr" 2>/dev/null || true
pkill -9 clewdr-bin 2>/dev/null || true
sleep 2

# 2. 可选：重置 DB
if $DO_RESET; then
  echo "==> 删除 $DB_FILE（重新初始化）..."
  rm -f "$DB_FILE" "${DB_FILE}-wal" "${DB_FILE}-shm"
  # 确认删除干净
  if [ -f "$DB_FILE" ]; then
    echo "==> 警告: $DB_FILE 仍存在，可能有进程占用"
    fuser -k "$DB_FILE" 2>/dev/null || true
    sleep 1
    rm -f "$DB_FILE" "${DB_FILE}-wal" "${DB_FILE}-shm"
  fi
fi

# 3. 可选：重建前端
if $DO_REBUILD; then
  echo "==> 构建前端..."
  cd frontend && npx vite build && cd ..
fi

# 4. 检查 static/ 存在
if [ ! -f static/index.html ]; then
  echo "==> static/index.html 不存在，先构建前端..."
  cd frontend && npx vite build && cd ..
fi

# 5. 启动后端
echo "==> 编译 + 启动后端..."
CLEWDR_IP="$BIND_IP" CLEWDR_PORT="$BIND_PORT" cargo run -- --db "$DB_FILE" 2>&1 &
PID=$!

# 6. 等待就绪
echo "==> 等待服务启动 (PID: $PID)..."
for i in $(seq 1 60); do
  if curl -s -o /dev/null -w "" "http://localhost:${BIND_PORT}/api/version" 2>/dev/null; then
    echo "==> 服务已就绪: http://localhost:${BIND_PORT}"
    exit 0
  fi
  sleep 1
done

echo "==> 启动超时，检查日志"
exit 1

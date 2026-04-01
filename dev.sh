#!/bin/bash
# dev.sh — ClewdR 开发调试一键脚本
# 用法: ./dev.sh [rebuild]
#   无参数: 仅重启后端（前端用已构建的 static/）
#   rebuild: 先构建前端再启动后端

set -e
cd "$(dirname "$0")"

# 1. 杀旧进程
pkill -9 -f "target/debug/clewdr" 2>/dev/null || true
pkill -9 -f "target/release/clewdr" 2>/dev/null || true
pkill -9 clewdr-bin 2>/dev/null || true
sleep 1

# 2. 可选：重建前端
if [ "$1" = "rebuild" ]; then
  echo "==> 构建前端..."
  cd frontend && npx vite build && cd ..
fi

# 3. 检查 static/ 存在
if [ ! -f static/index.html ]; then
  echo "==> static/index.html 不存在，先构建前端..."
  cd frontend && npx vite build && cd ..
fi

# 4. 启动后端（release profile for argon2 性能 + dev 功能）
echo "==> 编译 + 启动后端..."
CLEWDR_IP=0.0.0.0 CLEWDR_PORT=8484 cargo run 2>&1 &
PID=$!

# 5. 等待就绪
echo "==> 等待服务启动 (PID: $PID)..."
for i in $(seq 1 60); do
  if curl -s -o /dev/null -w "" http://localhost:8484/api/version 2>/dev/null; then
    echo "==> 服务已就绪: http://localhost:8484"
    echo "==> 公网: http://$(curl -s ifconfig.me 2>/dev/null || echo '?'):8484"
    exit 0
  fi
  sleep 1
done

echo "==> 启动超时，检查日志"
exit 1

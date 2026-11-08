#!/bin/bash
# Usage: correctness.sh <binary> <port> <outfile>
set -u
BIN="$1"; PORT="$2"; OUT="$3"
PORT=$PORT "$BIN" > "/tmp/server-$PORT.log" 2>&1 &
PID=$!
for i in $(seq 1 50); do curl -s -o /dev/null "http://127.0.0.1:$PORT/" && break || sleep 0.2; done
{
  echo "### GET /";            curl -s -i "http://127.0.0.1:$PORT/"
  echo; echo "### GET /big";  curl -s -i "http://127.0.0.1:$PORT/big"
  echo; echo "### GET /missing (expect 404)"; curl -s -i "http://127.0.0.1:$PORT/missing"
  echo; echo "### HEAD /";     curl -s -I "http://127.0.0.1:$PORT/"
  echo "### POST / (body echo route absent; record status)"; curl -s -i -X POST --data "hello-body" "http://127.0.0.1:$PORT/"
} > "$OUT" 2>&1
kill -TERM $PID 2>/dev/null; wait $PID 2>/dev/null
echo "done $OUT"

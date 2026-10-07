#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# The stream-driver contract against the fake worker, without gRPC: open
# TP_FOOTPRINT_STREAMS connections to /stream, wait for each to answer,
# print `held`, keep them open until stdin reaches end of file, exit 0.

set -Eeuo pipefail

host_port="${TP_FOOTPRINT_WORKER_URL#http://}"
host_port="${host_port%%/*}"
host="${host_port%:*}"
port="${host_port##*:}"
fds=()
for ((i = 0; i < TP_FOOTPRINT_STREAMS; i++)); do
  exec {fd}<>"/dev/tcp/${host}/${port}"
  printf 'GET /stream HTTP/1.0\r\n\r\n' >&"$fd"
  IFS= read -r -u "$fd" line
  [[ "$line" == "HTTP/1.1 200 OK"* ]] || { echo "stream $i: $line" >&2; exit 1; }
  fds+=("$fd")
done
echo held
cat >/dev/null
for fd in "${fds[@]}"; do
  exec {fd}>&-
done

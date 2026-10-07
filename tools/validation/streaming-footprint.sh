#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# Measure what the streaming feature adds to the serving worker: stripped
# installed size, idle resident set, and resident set with the declared
# number of synthetic streams held open. Both inputs are packaged builds of
# the same release configuration, one with the feature and one without. The
# budgets come from streaming-footprint-thresholds.json beside this script
# and are copied into the record as read; streaming_footprint_record.py
# derives the verdict from the raw captures and refuses a record it cannot
# derive. Linux only: the resident set is read from /proc.
#
# Usage:
#   streaming-footprint.sh --with-streaming FILE.deb --without-streaming FILE.deb \
#     --out DIR [--stream-driver COMMAND] [options]
#
# The stream driver, when given, runs once per run with the worker ready and
# TP_FOOTPRINT_WORKER_URL, TP_FOOTPRINT_WORKER_PID and TP_FOOTPRINT_STREAMS in
# its environment, its stdin a pipe. It opens that many streams, prints `held`
# on stdout, keeps them open until its stdin reaches end of file, then exits
# 0. Without a driver the steady-state case is recorded as not run.

set -Eeuo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
record_tool="${here}/streaming_footprint_record.py"
thresholds="${here}/streaming-footprint-thresholds.json"
worker_path=usr/lib/tensorplate/tensorplate-serving
config_path=etc/tensorplate/serving_worker.json

with_pkg=""
without_pkg=""
out=""
driver=""
settle_seconds=5
samples=10
sample_interval=1
ready_timeout=60
provenance=recorded
python=python3

usage() {
  sed -n '2,/^$/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' | sed '1d'
  cat <<EOF

Options:
  --with-streaming FILE     tensorplate-serving .deb built with the streaming feature
  --without-streaming FILE  the same configuration built without it
  --out DIR                 record directory; must not hold captures already
  --stream-driver COMMAND   executable that holds the declared streams open (see above)
  --settle-seconds S        wait after ready, and after the driver holds, before sampling (${settle_seconds})
  --samples N               VmRSS samples per phase (${samples})
  --sample-interval S       seconds between samples (${sample_interval})
  --ready-timeout S         seconds to wait for /health to report ready (${ready_timeout})
  --provenance KIND         recorded (default) or synthetic, for the tool's own tests
  --python PATH             interpreter for the record tool (${python})
EOF
}

die() {
  printf 'streaming-footprint: %s\n' "$*" >&2
  exit 2
}

while (($#)); do
  case "$1" in
    --with-streaming) with_pkg="${2:?}"; shift 2 ;;
    --without-streaming) without_pkg="${2:?}"; shift 2 ;;
    --out) out="${2:?}"; shift 2 ;;
    --stream-driver) driver="${2:?}"; shift 2 ;;
    --settle-seconds) settle_seconds="${2:?}"; shift 2 ;;
    --samples) samples="${2:?}"; shift 2 ;;
    --sample-interval) sample_interval="${2:?}"; shift 2 ;;
    --ready-timeout) ready_timeout="${2:?}"; shift 2 ;;
    --provenance) provenance="${2:?}"; shift 2 ;;
    --python) python="${2:?}"; shift 2 ;;
    -h | --help) usage; exit 0 ;;
    *) usage >&2; die "unknown argument: $1" ;;
  esac
done

[[ -n "$with_pkg" && -n "$without_pkg" && -n "$out" ]] ||
  { usage >&2; die "--with-streaming, --without-streaming and --out are required"; }
[[ -f "$with_pkg" ]] || die "no such package: $with_pkg"
[[ -f "$without_pkg" ]] || die "no such package: $without_pkg"
if [[ "$with_pkg" -ef "$without_pkg" ]]; then die "both sides name the same package file"; fi
[[ -z "$driver" || -x "$driver" ]] || die "stream driver is not executable: $driver"
[[ "$samples" =~ ^[1-9][0-9]*$ ]] || die "--samples must be a positive integer"
[[ "$ready_timeout" =~ ^[1-9][0-9]*$ ]] || die "--ready-timeout must be a positive integer"
[[ "$settle_seconds" =~ ^[0-9]+(\.[0-9]+)?$ ]] || die "--settle-seconds must be a non-negative number"
[[ "$sample_interval" =~ ^[0-9]+(\.[0-9]+)?$ ]] || die "--sample-interval must be a non-negative number"
[[ "$provenance" == recorded || "$provenance" == synthetic ]] ||
  die "--provenance must be recorded or synthetic"
[[ -d /proc/self ]] || die "this harness reads /proc and runs on Linux only"
for tool in dpkg-deb dpkg strip size stat od uname sha256sum "$python"; do
  command -v "$tool" >/dev/null 2>&1 || die "$tool is required and not on PATH"
done
[[ -r "$thresholds" ]] || die "thresholds file is missing: $thresholds"
[[ -r "$record_tool" ]] || die "record tool is missing: $record_tool"

captures="${out}/captures"
if [[ -e "$captures" ]]; then
  die "$captures exists; a record is captured into an empty directory, never over another"
fi
mkdir -p "$captures"

# The declared run and stream counts; the record tool validates the file's shape.
read -r runs streams < <("$python" - "$thresholds" <<'PY'
import json, sys
t = json.load(open(sys.argv[1], encoding="utf-8"))
print(t["runs"], t["budgets"]["steady_rss"]["streams"])
PY
) || die "cannot read the run and stream counts from $thresholds"
[[ "$runs" =~ ^[1-9][0-9]*$ && "$streams" =~ ^[1-9][0-9]*$ ]] ||
  die "thresholds do not declare positive run and stream counts"
cp "$thresholds" "${captures}/thresholds.json"
printf 'machine %s\nkernel %s\n' "$(uname -m)" "$(uname -r)" >"${captures}/host.txt"
host_arch="$(dpkg --print-architecture)"

worker_pid=""
driver_pid=""
driver_in=""
work=""

# shellcheck disable=SC2329  # invoked by the EXIT trap
cleanup() {
  if [[ -n "$driver_in" ]]; then exec {driver_in}>&- || true; driver_in=""; fi
  if [[ -n "$driver_pid" ]] && kill -0 "$driver_pid" 2>/dev/null; then kill "$driver_pid" 2>/dev/null || true; fi
  if [[ -n "$worker_pid" ]] && kill -0 "$worker_pid" 2>/dev/null; then kill -KILL "$worker_pid" 2>/dev/null || true; fi
  if [[ -n "$work" ]]; then rm -rf "$work"; fi
}
trap cleanup EXIT
# A command that fails outside a guard is an infrastructure failure, never a verdict.
trap 'exit 2' ERR

is_elf() {
  [[ "$(head -c 4 "$1" | od -An -tx1 | tr -d ' \n')" == "7f454c46" ]]
}

# A file from the package's extracted tree, the stripped copy's size beside the installed one.
measure_elf_files() {
  local root="$1" run_dir="$2" stripped_dir file rel bytes stripped
  stripped_dir="${run_dir}/stripped"
  mkdir -p "$stripped_dir"
  : >"${run_dir}/elf-files.tsv"
  : >"${run_dir}/size.txt"
  while IFS= read -r file; do
    is_elf "$file" || continue
    rel="${file#"${root}"/}"
    bytes="$(stat -c %s "$file")"
    cp "$file" "${stripped_dir}/${rel//\//__}"
    strip --strip-all "${stripped_dir}/${rel//\//__}" ||
      die "strip failed on ${rel}"
    stripped="$(stat -c %s "${stripped_dir}/${rel//\//__}")"
    printf '%s\t%s\t%s\n' "$rel" "$bytes" "$stripped" >>"${run_dir}/elf-files.tsv"
    # From inside the directory, so the capture names the file and not a temporary path.
    { printf '# %s\n' "$rel"; (cd "$stripped_dir" && size "${rel//\//__}"); } >>"${run_dir}/size.txt"
  done < <(find "$root" -type f | LC_ALL=C sort)
  rm -rf "$stripped_dir"
}

# <time_ns>\t<VmRSS kib> per line; the worker must be alive for every sample.
sample_vmrss() {
  local pid="$1" file="$2" phase="$3" i kib
  : >"$file"
  for ((i = 0; i < samples; i++)); do
    kib="$(awk '/^VmRSS:/ { print $2 }' "/proc/${pid}/status" 2>/dev/null || true)"
    [[ "$kib" =~ ^[0-9]+$ ]] || die "worker ${pid} has no VmRSS during the ${phase} phase (it exited?)"
    printf '%s\t%s\n' "$(date +%s%N)" "$kib" >>"$file"
    if ((i + 1 < samples)); then sleep "$sample_interval"; fi
  done
}

stop_worker() {
  local pid="$1" file="$2" status=0 i
  kill -TERM "$pid" 2>/dev/null || true
  for ((i = 0; i < 100; i++)); do
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.1
  done
  if kill -0 "$pid" 2>/dev/null; then
    kill -KILL "$pid" 2>/dev/null || true
    echo "killed after 10s" >>"$file"
  fi
  wait "$pid" || status=$?
  printf 'exit %s\n' "$status" >>"$file"
  worker_pid=""
}

run_driver() {
  local run_dir="$1" url="$2" pid="$3" fifo i held=""
  fifo="${run_dir}/driver-stdin.fifo"
  mkfifo "$fifo"
  TP_FOOTPRINT_WORKER_URL="$url" TP_FOOTPRINT_WORKER_PID="$pid" TP_FOOTPRINT_STREAMS="$streams" \
    "$driver" <"$fifo" >"${run_dir}/driver.out" 2>"${run_dir}/driver.log" &
  driver_pid=$!
  exec {driver_in}>"$fifo"
  rm -f "$fifo"
  for ((i = 0; i < ready_timeout * 10; i++)); do
    if grep -qx held "${run_dir}/driver.out" 2>/dev/null; then held=1; break; fi
    kill -0 "$driver_pid" 2>/dev/null || break
    sleep 0.1
  done
  [[ -n "$held" ]] || die "stream driver did not report 'held' (see ${run_dir}/driver.log)"
  sleep "$settle_seconds"
  sample_vmrss "$pid" "${run_dir}/steady-vmrss.tsv" "steady-state"
  printf '%s\n' "$streams" >"${run_dir}/steady-streams.txt"
  exec {driver_in}>&-
  driver_in=""
  wait "$driver_pid" || die "stream driver exited $? after releasing its streams"
  driver_pid=""
}

measure_side() {
  local side="$1" pkg="$2" side_dir arch version run run_dir root worker port url
  side_dir="${captures}/${side}"
  mkdir -p "$side_dir"
  arch="$(dpkg-deb -f "$pkg" Architecture)"
  version="$(dpkg-deb -f "$pkg" Version)"
  [[ "$arch" == "$host_arch" ]] ||
    die "${side}: package architecture ${arch} is not this host's ${host_arch}; the worker must run here"
  [[ -n "$version" ]] || die "${side}: package has no Version"
  {
    printf 'file %s\n' "$(basename "$pkg")"
    printf 'sha256 %s\n' "$(sha256sum "$pkg" | awk '{ print $1 }')"
    printf 'version %s\n' "$version"
    printf 'architecture %s\n' "$arch"
  } >"${side_dir}/package.txt"

  for ((run = 1; run <= runs; run++)); do
    run_dir="${side_dir}/run-${run}"
    mkdir -p "$run_dir"
    work="$(mktemp -d)"
    root="${work}/root"
    mkdir -p "$root"
    dpkg-deb -x "$pkg" "$root"
    worker="${root}/${worker_path}"
    [[ -f "$worker" ]] || die "${side}: package ships no ${worker_path}"
    is_elf "$worker" || die "${side}: ${worker_path} is not an ELF executable"
    [[ -f "${root}/${config_path}" ]] || die "${side}: package ships no ${config_path}"
    chmod u+x "$worker"
    if ((run == 1)); then
      "$worker" --version >"${side_dir}/version-output.txt" 2>>"${side_dir}/version-output.log" ||
        die "${side}: ${worker_path} --version failed (see ${side_dir}/version-output.log)"
    fi
    measure_elf_files "$root" "$run_dir"

    port="$("$python" "$record_tool" free-port)"
    url="http://127.0.0.1:${port}"
    "$worker" --config "${root}/${config_path}" --bind-host 127.0.0.1 --bind-port "$port" \
      >"${run_dir}/worker.log" 2>&1 &
    worker_pid=$!
    "$python" "$record_tool" wait-ready --url "${url}/health" --timeout "$ready_timeout" \
      --out "${run_dir}/health.json" ||
      die "${side} run ${run}: worker not ready (see ${run_dir}/worker.log)"
    sleep "$settle_seconds"
    sample_vmrss "$worker_pid" "${run_dir}/idle-vmrss.tsv" "idle"
    if [[ -n "$driver" ]]; then
      run_driver "$run_dir" "$url" "$worker_pid"
    else
      echo "no stream driver was given" >"${run_dir}/steady-not-run.txt"
    fi
    stop_worker "$worker_pid" "${run_dir}/worker-exit.txt"
    rm -rf "$work"
    work=""
    printf 'streaming-footprint: %s run %s of %s captured\n' "$side" "$run" "$runs"
  done
}

measure_side with_streaming "$with_pkg"
measure_side without_streaming "$without_pkg"

source_commit="$(git -C "$here" rev-parse HEAD 2>/dev/null || true)"
assemble_args=(assemble --captures "$captures" --out "${out}/record.json" --provenance "$provenance")
if [[ "$source_commit" =~ ^[0-9a-f]{40}$ ]]; then
  assemble_args+=(--source-commit "$source_commit")
fi
"$python" "$record_tool" "${assemble_args[@]}"

# The summary tool refuses only a record the check refuses, which is reported next.
summary_status=0
"$python" "$record_tool" summary "${out}/record.json" --out "${out}/summary.json" 2>/dev/null ||
  summary_status=$?
status=0
"$python" "$record_tool" check "${out}/record.json" || status=$?
if ((summary_status != 0 && status != 2)); then
  die "the summary was not written (exit ${summary_status}) for a record the check accepted"
fi
exit "$status"

#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
#
# Stands in for tensorplate-serving, invoked as `--config <path>`. A
# deployment id `fail-<name>` replays `<name>.stderr` and exits with the
# worker's load-error status; `late-<name>` exits first and lets a child
# replay it afterwards; `again-<name>` stays alive the first time it is
# started and fails like `fail-<name>` after that; `exit-<n>` exits <n>
# silently; `mute-<name>` stays alive and never answers its control socket;
# any other id stays alive answering it (control-responder.py) without
# listening.
id=$(sed -n 's/.*"model_id": "\([^"]*\)".*/\1/p' "$2")
fixtures=$(dirname "$0")
case "$id" in
  fail-*)
    cat "$fixtures/${id#fail-}.stderr" >&2
    exit 65
    ;;
  late-*)
    (sleep 0.3; cat "$fixtures/${id#late-}.stderr" >&2) &
    exit 65
    ;;
  again-*)
    marker="$(dirname "$2")/$id.started"
    if [ -e "$marker" ]; then
      cat "$fixtures/${id#again-}.stderr" >&2
      exit 65
    fi
    : > "$marker"
    ;;
  exit-*)
    exit "${id#exit-}"
    ;;
  mute-*)
    exec sleep 600
    ;;
esac
exec python3 "$fixtures/control-responder.py" "$2"

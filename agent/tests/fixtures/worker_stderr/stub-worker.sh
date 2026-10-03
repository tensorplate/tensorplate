#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
#
# Stands in for tensorplate-serving, invoked as `--config <path>`. A
# deployment id `fail-<name>` replays `<name>.stderr` and exits with the
# worker's load-error status; `exit-<n>` exits <n> silently; any other id
# stays alive without listening.
id=$(sed -n 's/.*"model_id": "\([^"]*\)".*/\1/p' "$2")
case "$id" in
  fail-*)
    cat "$(dirname "$0")/${id#fail-}.stderr" >&2
    exit 65
    ;;
  exit-*)
    exit "${id#exit-}"
    ;;
esac
exec sleep 600

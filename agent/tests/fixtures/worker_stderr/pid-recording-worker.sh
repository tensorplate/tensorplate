#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
#
# stub-worker.sh, started after this process left its id in `<config>.pid`.
# The id stays the stand-in's: each step replaces the process.
echo "$$" > "$2.pid"
exec "$(dirname "$0")/stub-worker.sh" "$@"

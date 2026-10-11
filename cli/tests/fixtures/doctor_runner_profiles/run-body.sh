#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
#
# A test interpreter: linked at an interpreter's path, it runs the shell
# script written next to that link as `<link>.body`, with the same
# arguments. The executable is never opened for writing, so no concurrent
# fork can hold it busy (ETXTBSY) when it is executed.
exec /bin/sh "$0.body" "$@"

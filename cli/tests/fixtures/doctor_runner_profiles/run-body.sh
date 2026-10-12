#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
#
# A test interpreter: linked at an interpreter's path, it runs the shell
# script written next to that link as `<link>.body`, with the same
# arguments. The executable is never opened for writing, so no concurrent
# fork can hold it busy (ETXTBSY) when it is executed.
#
# The protocol crate's tests keep their own copy of this script:
# protocol/rust/tests/fixtures/test_script/run-body.sh.
exec /bin/sh "$0.body" "$@"

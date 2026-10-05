# tensorplate probe: interpreter
# SPDX-License-Identifier: Apache-2.0
#
# Run by the backend probe in a runner profile's interpreter, which may be
# older than the backend's minimum: no syntax newer than Python 2.7.
import json
import sys

facts = {
    "version": "%d.%d.%d" % sys.version_info[:3],  # noqa: UP031
    "sys_version": sys.version,
    "prefix": sys.prefix,
}
print(json.dumps(facts))

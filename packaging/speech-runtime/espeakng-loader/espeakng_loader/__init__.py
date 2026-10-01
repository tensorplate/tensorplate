# SPDX-License-Identifier: Apache-2.0
"""The distribution's espeak-ng, under the upstream loader's module name.

The upstream wheel bundles its own libespeak-ng and data. The speech runtime
depends on the libespeak-ng1 and espeak-ng-data packages instead and installs
this module in the upstream wheel's place.
"""

_LIBRARY = "/usr/lib/x86_64-linux-gnu/libespeak-ng.so.1"
_DATA = "/usr/lib/x86_64-linux-gnu/espeak-ng-data"


def get_library_path() -> str:
    return _LIBRARY


def get_data_path() -> str:
    return _DATA

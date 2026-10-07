# Native closure SBOM fixtures

`install-tree/` is what vcpkg wrote for the release configuration: the
manifest in this repository at its `builtin-baseline`, feature
`streaming-grpc`, triplet `x64-linux`, installed on 2026-10-07 in an Ubuntu
22.04 container with clang 15 by vcpkg `2026-07-27` (a cold build, no
binary cache). It holds the two things `tools/release/native-sbom.py`
reads from an install tree and nothing else, for all thirteen installed ports
(the closure's nine, gtest, and vcpkg's own helper ports):

- `vcpkg/status`, as written;
- each installed port's `x64-linux/share/<port>/vcpkg.spdx.json`, with its
  `files` array and the relationships to those files removed. The tool
  reads a document's `packages` and `creationInfo` only, and the file lists
  are about a megabyte across the ports.

Document namespace UUIDs are synthetic version-4 values so recorded fixtures contain no random document identifiers.

The headers, libraries and tools of the tree are not kept. The `Abi` values
in the status file are those of that container's build and match no
published archive. Recorded on a throwaway container: the files name no
host, account or path.

To record again after the baseline or the feature changes, run
`vcpkg install --triplet x64-linux --x-feature=streaming-grpc
--x-install-root=<dir>` from a vcpkg checkout at the manifest's baseline,
copy the status file, reduce each SPDX document and substitute its namespace UUID the same way.

`native-sbom.py control` generates synthetic vulnerable-version documents
from the identifier table, one package per controlled vendor/product pair.
They are scanner controls, not recordings of a build. `check-control` requires
each pair to match its native advisory through the CPE matcher and refuses
unexpected control artifacts. nlohmann-json retains its two original CPEs as
`LOOKUP_WITHOUT_POSITIVE_CONTROL`: no native historical advisory exists in the
inspected database, so no vulnerable version or advisory is invented for it.
re2 and utf8-range remain `UNSCANNED`. The collected document records all three
coverage states separately.

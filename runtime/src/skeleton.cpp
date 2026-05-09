// SPDX-License-Identifier: Apache-2.0
//
// Anchor translation unit for `tp_runtime`.
//
// V01-E01-F02 only needs the static library to have at least one
// translation unit so it can be linked. Real runtime code lands in V01-E02
// onward. The function below is intentionally not declared in any header;
// it exists solely to give `tp_runtime` a symbol on platforms that warn or
// error on empty archives.

namespace tensorplate::internal {

void runtime_link_anchor() noexcept {}

}  // namespace tensorplate::internal

// SPDX-License-Identifier: Apache-2.0
//
// tensorplate-serving entrypoint placeholder.
//
// V01-E01-F02 wires the binary so CMake target names, link order, and
// install paths exist. The real HTTP endpoint, request router, scheduler
// integration, and graceful shutdown land in V01-E07.

#include "tensorplate/version.hpp"

#include <cstdlib>
#include <iostream>
#include <string_view>

namespace {

int print_version() {
  std::cout << "tensorplate-serving " << tensorplate::kRuntimeVersion << '\n'
            << "protocol " << tensorplate::kProtocolVersion << '\n'
            << "bundle-format " << tensorplate::kBundleFormatVersion << '\n';
  return EXIT_SUCCESS;
}

int print_usage() {
  std::cerr << "usage: tensorplate-serving [--version]\n"
            << "  V01-E01-F02 scaffolding only; serving endpoint lands in "
               "V01-E07.\n";
  return EXIT_FAILURE;
}

}  // namespace

int main(int argc, char** argv) {
  if (argc == 2 && std::string_view(argv[1]) == "--version") {
    return print_version();
  }
  if (argc == 1) {
    return print_version();
  }
  return print_usage();
}

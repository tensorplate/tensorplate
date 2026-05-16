// SPDX-License-Identifier: Apache-2.0
//
// V01-E07: tensorplate-serving entrypoint.
//
// Parses the serving config (from --config <path>, --config-json
// <inline JSON>, or defaults), constructs the ServingWorker
// composition root, registers signal handlers for graceful shutdown,
// and runs `serve_forever`.

#include <cerrno>
#include <csignal>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <fstream>
#include <iostream>
#include <memory>
#include <sstream>
#include <string>
#include <string_view>

#include "tensorplate/serving/config.hpp"
#include "tensorplate/serving/worker.hpp"
#include "tensorplate/version.hpp"

namespace {

// The signal handler reaches the worker through this pointer. It is
// scoped to the lifetime of `main()` and reset before the worker is
// destroyed; non-const access is required so `shutdown()` can be
// invoked from the signal-handler path.
// NOLINTNEXTLINE(cppcoreguidelines-avoid-non-const-global-variables)
tensorplate::ServingWorker* g_worker = nullptr;

void handle_signal(int signum) {
  if (g_worker != nullptr) {
    g_worker->shutdown(signum == SIGINT ? "SIGINT" : "SIGTERM");
  }
}

void install_signal_handlers() {
  struct sigaction sa {};
  sa.sa_handler = handle_signal;
  sigemptyset(&sa.sa_mask);
  sa.sa_flags = SA_RESTART;
  sigaction(SIGINT, &sa, nullptr);
  sigaction(SIGTERM, &sa, nullptr);
  // Ignore SIGPIPE; HTTP server callers see EPIPE on send instead.
  std::signal(SIGPIPE, SIG_IGN);
}

void print_version() {
  std::cout << "tensorplate-serving " << tensorplate::kRuntimeVersion << '\n'
            << "protocol " << tensorplate::kProtocolVersion << '\n'
            << "bundle-format " << tensorplate::kBundleFormatVersion << '\n';
}

void print_usage() {
  std::cout << "usage: tensorplate-serving [options]\n"
            << "  --version                Print version and exit.\n"
            << "  --config <path>          Load JSON config from a file.\n"
            << "  --config-json <inline>   Load JSON config from an inline string.\n"
            << "  --bind-host <host>       Override bind.host (default 127.0.0.1).\n"
            << "  --bind-port <port>       Override bind.port (default 0 = ephemeral).\n"
            << "  --mock                   Use the built-in mock session (default).\n"
            << "  --help                   Print this message and exit.\n";
}

// NOLINTNEXTLINE(readability-function-cognitive-complexity)
tensorplate::Result<tensorplate::ServingConfig> load_config_from_args(int argc, char** argv,
                                                                      bool& help) {
  std::string config_path;
  std::string config_json;
  std::string bind_host;
  std::optional<int> bind_port;
  bool force_mock = false;
  for (int i = 1; i < argc; ++i) {
    std::string_view a = argv[i];
    if (a == "--help" || a == "-h") {
      help = true;
    } else if (a == "--version") {
      // handled separately
      help = false;
    } else if (a == "--config" && i + 1 < argc) {
      config_path = argv[++i];
    } else if (a == "--config-json" && i + 1 < argc) {
      config_json = argv[++i];
    } else if (a == "--bind-host" && i + 1 < argc) {
      bind_host = argv[++i];
    } else if (a == "--bind-port" && i + 1 < argc) {
      try {
        bind_port = std::stoi(argv[++i]);
      } catch (...) {
        return tensorplate::unexpected(tensorplate::Error::Code::ConfigInvalid, "bad --bind-port");
      }
    } else if (a == "--mock") {
      force_mock = true;
    } else if (a.substr(0, 2) == "--") {
      return tensorplate::unexpected(tensorplate::Error::Code::ConfigInvalid,
                                     std::string{"unknown flag "} + std::string(a));
    }
  }
  tensorplate::ServingConfig cfg;
  if (!config_path.empty()) {
    std::ifstream f(config_path);
    if (!f.is_open()) {
      return tensorplate::unexpected(tensorplate::Error::Code::ConfigInvalid,
                                     std::string{"cannot open config file: "} + config_path);
    }
    std::stringstream ss;
    ss << f.rdbuf();
    auto r = tensorplate::ServingConfig::parse_json(ss.str());
    if (!r) {
      return tensorplate::unexpected(r.error());
    }
    cfg = std::move(r).value();
  } else if (!config_json.empty()) {
    auto r = tensorplate::ServingConfig::parse_json(config_json);
    if (!r) {
      return tensorplate::unexpected(r.error());
    }
    cfg = std::move(r).value();
  }
  if (!bind_host.empty()) {
    cfg.bind.host = bind_host;
  }
  if (bind_port.has_value()) {
    cfg.bind.port = static_cast<std::uint16_t>(*bind_port);
  }
  if (force_mock) {
    cfg.deployment.use_mock_session = true;
  }
  if (auto v = cfg.validate(); !v) {
    return tensorplate::unexpected(v.error());
  }
  return cfg;
}

}  // namespace

int main(int argc, char** argv) {
  for (int i = 1; i < argc; ++i) {
    std::string_view a = argv[i];
    if (a == "--version") {
      print_version();
      return EXIT_SUCCESS;
    }
    if (a == "--help" || a == "-h") {
      print_usage();
      return EXIT_SUCCESS;
    }
  }
  bool help = false;
  auto cfg_r = load_config_from_args(argc, argv, help);
  if (help) {
    print_usage();
    return EXIT_SUCCESS;
  }
  if (!cfg_r) {
    std::cerr << "config error: " << cfg_r.error().message << '\n';
    return static_cast<int>(tensorplate::ServingExitCode::ConfigError);
  }
  install_signal_handlers();
  auto worker_r = tensorplate::ServingWorker::create(std::move(cfg_r).value());
  if (!worker_r) {
    std::cerr << "worker init failed: " << worker_r.error().message << '\n';
    return static_cast<int>(tensorplate::ServingExitCode::LoadError);
  }
  auto worker = std::move(worker_r).value();
  g_worker = worker.get();
  auto code = worker->serve_forever();
  g_worker = nullptr;
  return static_cast<int>(code);
}

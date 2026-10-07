// SPDX-License-Identifier: Apache-2.0
//
// V01-E07: tensorplate-serving entrypoint.
//
// Parses the serving config (from --config <path>, --config-json
// <inline JSON>, or defaults), constructs the ServingWorker
// composition root, registers signal handlers for graceful shutdown,
// and waits for signals on the main thread.

#include <fcntl.h>
#include <poll.h>
#include <unistd.h>

#include <array>
#include <cerrno>
#include <chrono>
#include <csignal>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <fstream>
#include <iostream>
#include <memory>
#include <nlohmann/json.hpp>
#include <sstream>
#include <string>
#include <string_view>

#include "tensorplate/core/error.hpp"
#include "tensorplate/serving/config.hpp"
#include "tensorplate/serving/worker.hpp"
#include "tensorplate/version.hpp"

namespace {

// Set before installing handlers; the pipe outlives every worker thread.
// NOLINTNEXTLINE(cppcoreguidelines-avoid-non-const-global-variables)
volatile sig_atomic_t g_signal_write_fd = -1;

void handle_signal(int signum) {
  const int saved_errno = errno;
  const auto signal = static_cast<unsigned char>(signum);
  // A full pipe already holds a shutdown request. Never wait in a handler.
  while (::write(g_signal_write_fd, &signal, sizeof(signal)) < 0 && errno == EINTR) {
  }
  errno = saved_errno;
}

class SignalWakeup {
 public:
  SignalWakeup() = default;
  SignalWakeup(const SignalWakeup&) = delete;
  SignalWakeup& operator=(const SignalWakeup&) = delete;

  ~SignalWakeup() {
    for (const auto& signal : handlers_) {
      if (signal.installed) {
        ::sigaction(signal.number, &signal.previous, nullptr);
      }
    }
    for (const int fd : pipe_) {
      if (fd >= 0) {
        ::close(fd);
      }
    }
  }

  tensorplate::Result<void> install() {
    if (::pipe(pipe_.data()) != 0) {
      return tensorplate::unexpected(tensorplate::Error::Code::Internal,
                                     "serving worker: cannot create signal pipe");
    }
    for (auto& fd : pipe_) {
      // Even a process started with closed standard descriptors keeps this
      // pipe private, so a sidecar cannot inherit an end as stdin or stderr.
      // NOLINTNEXTLINE(cppcoreguidelines-pro-type-vararg): POSIX descriptor operation.
      const int moved = ::fcntl(fd, F_DUPFD_CLOEXEC, STDERR_FILENO + 1);
      if (moved < 0) {
        return tensorplate::unexpected(tensorplate::Error::Code::Internal,
                                       "serving worker: cannot move signal pipe");
      }
      ::close(fd);
      fd = moved;
      // NOLINTNEXTLINE(cppcoreguidelines-pro-type-vararg): POSIX descriptor operation.
      if (::fcntl(fd, F_SETFL, O_NONBLOCK) != 0) {
        return tensorplate::unexpected(tensorplate::Error::Code::Internal,
                                       "serving worker: cannot make signal pipe nonblocking");
      }
    }
    g_signal_write_fd = pipe_[1];
    struct sigaction action {};
    ::sigemptyset(&action.sa_mask);
    ::sigaddset(&action.sa_mask, SIGINT);
    ::sigaddset(&action.sa_mask, SIGTERM);
    action.sa_flags = SA_RESTART;
    for (auto& signal : handlers_) {
      action.sa_handler = signal.number == SIGPIPE ? SIG_IGN : handle_signal;
      if (::sigaction(signal.number, &action, &signal.previous) != 0) {
        return tensorplate::unexpected(tensorplate::Error::Code::Internal,
                                       "serving worker: cannot install signal handler");
      }
      signal.installed = true;
    }
    return {};
  }

  tensorplate::Result<int> wait(int timeout_ms) const {
    pollfd pending{pipe_[0], POLLIN, 0};
    int ready = 0;
    do {
      ready = ::poll(&pending, 1, timeout_ms);
    } while (ready < 0 && errno == EINTR);
    if (ready == 0) {
      return 0;
    }
    unsigned char signal = 0;
    ssize_t count = -1;
    if (ready > 0) {
      do {
        count = ::read(pipe_[0], &signal, sizeof(signal));
      } while (count < 0 && errno == EINTR);
    }
    if (count != sizeof(signal)) {
      return tensorplate::unexpected(tensorplate::Error::Code::Internal,
                                     "serving worker: cannot read signal pipe");
    }
    return signal;
  }

 private:
  struct SavedHandler {
    int number;
    struct sigaction previous {};
    bool installed = false;
  };
  std::array<SavedHandler, 3> handlers_{{{SIGINT}, {SIGTERM}, {SIGPIPE}}};
  std::array<int, 2> pipe_{-1, -1};
};

tensorplate::ServingExitCode run_until_signal(tensorplate::ServingWorker& worker,
                                              const SignalWakeup& signals) {
  // A signal received during load must not be lost or publish a new listener.
  auto signal = signals.wait(0);
  if (signal && *signal == 0) {
    if (auto started = worker.start(); !started) {
      return started.error().code == tensorplate::Error::Code::ConfigInvalid
                 ? tensorplate::ServingExitCode::ConfigError
                 : tensorplate::ServingExitCode::ServeError;
    }
    signal = signals.wait(-1);
  }
  worker.shutdown(!signal ? "signal wait failed" : (*signal == SIGINT ? "SIGINT" : "SIGTERM"));
  const auto code = worker.stop();
  return signal ? code : tensorplate::ServingExitCode::Internal;
}

// The last stderr line of a worker that could not start. The agent reads the
// code from it to answer the deploy that spawned this worker, so the line is
// written whether or not the config enables stderr logs.
void report_startup_failure(const tensorplate::Error& error) noexcept {
  try {
    nlohmann::json record;
    record["ts_ns"] = std::chrono::duration_cast<std::chrono::nanoseconds>(
                          std::chrono::steady_clock::now().time_since_epoch())
                          .count();
    record["level"] = "error";
    record["component"] = "serving";
    record["message"] = "worker startup failed";
    record["fields"] = {{"code", std::string{tensorplate::to_string(error.code)}},
                        {"message", error.message}};
    std::cerr << record.dump(-1, ' ', false, nlohmann::json::error_handler_t::replace) << std::endl;
  } catch (...) {
    // The exit status still tells the agent the worker failed to start.
    std::fputs("worker startup failed\n", stderr);
  }
}

void print_version() {
  // The fourth line is the one place an installed worker says whether streaming
  // support was compiled in; the first three keep their shape.
  std::cout << "tensorplate-serving " << tensorplate::kRuntimeVersion << '\n'
            << "protocol " << tensorplate::kProtocolVersion << '\n'
            << "bundle-format " << tensorplate::kBundleFormatVersion << '\n'
#if TP_ENABLE_STREAMING_GRPC
            << "streaming-grpc on\n";
#else
            << "streaming-grpc off\n";
#endif
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

int main(int argc, char** argv) try {
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
    report_startup_failure(cfg_r.error());
    return static_cast<int>(tensorplate::ServingExitCode::ConfigError);
  }
  SignalWakeup signals;
  if (auto installed = signals.install(); !installed) {
    report_startup_failure(installed.error());
    return static_cast<int>(tensorplate::ServingExitCode::Internal);
  }
  auto worker_r = tensorplate::ServingWorker::create(std::move(cfg_r).value());
  if (!worker_r) {
    report_startup_failure(worker_r.error());
    return static_cast<int>(tensorplate::ServingExitCode::LoadError);
  }
  auto worker = std::move(worker_r).value();
  const auto code = run_until_signal(*worker, signals);
  return static_cast<int>(code);
} catch (...) {
  // The process boundary must still report a failure if allocation fails.
  std::fputs("worker failed unexpectedly\n", stderr);
  return static_cast<int>(tensorplate::ServingExitCode::Internal);
}

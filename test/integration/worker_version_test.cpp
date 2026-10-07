// SPDX-License-Identifier: Apache-2.0
//
// `tensorplate-serving --version` is the one place an installed worker says
// whether streaming support is compiled in. Both build flavours run this.

#include <gtest/gtest.h>
#include <sys/wait.h>
#include <unistd.h>

#include <array>
#include <cstdlib>
#include <sstream>
#include <string>
#include <vector>

#ifdef TP_SERVING_WORKER_BINARY

namespace {

struct Captured {
  int status = -1;
  std::string stdout_text;
};

Captured run_version() {
  std::array<int, 2> fds{};
  if (::pipe(fds.data()) != 0) {
    return {};
  }
  const pid_t pid = ::fork();
  if (pid < 0) {
    return {};
  }
  if (pid == 0) {
    ::close(fds[0]);
    if (::dup2(fds[1], STDOUT_FILENO) < 0) {
      ::_exit(126);
    }
    ::close(fds[1]);
    ::execl(TP_SERVING_WORKER_BINARY, TP_SERVING_WORKER_BINARY, "--version",
            static_cast<char*>(nullptr));
    ::_exit(127);
  }
  ::close(fds[1]);
  Captured captured;
  std::array<char, 512> buffer{};
  for (;;) {
    const ssize_t got = ::read(fds[0], buffer.data(), buffer.size());
    if (got <= 0) {
      break;
    }
    captured.stdout_text.append(buffer.data(), static_cast<std::size_t>(got));
  }
  ::close(fds[0]);
  int status = 0;
  ::waitpid(pid, &status, 0);
  captured.status = WIFEXITED(status) ? WEXITSTATUS(status) : -1;
  return captured;
}

std::vector<std::string> lines_of(const std::string& text) {
  std::vector<std::string> lines;
  std::istringstream in(text);
  std::string line;
  while (std::getline(in, line)) {
    lines.push_back(line);
  }
  return lines;
}

}  // namespace

TEST(WorkerVersion, ReportsStreamingSupport) {
  const Captured captured = run_version();
  ASSERT_EQ(captured.status, 0) << captured.stdout_text;
  const auto lines = lines_of(captured.stdout_text);
  ASSERT_EQ(lines.size(), 4U) << captured.stdout_text;
  EXPECT_EQ(lines[0].rfind("tensorplate-serving ", 0), 0U) << lines[0];
  EXPECT_EQ(lines[1].rfind("protocol ", 0), 0U) << lines[1];
  EXPECT_EQ(lines[2].rfind("bundle-format ", 0), 0U) << lines[2];
#if TP_ENABLE_STREAMING_GRPC
  EXPECT_EQ(lines[3], "streaming-grpc on");
#else
  EXPECT_EQ(lines[3], "streaming-grpc off");
#endif
}

#endif  // TP_SERVING_WORKER_BINARY

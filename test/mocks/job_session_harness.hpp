// SPDX-License-Identifier: Apache-2.0
//
// The Python/PyTorch adapter's session together with its job channel, for
// tests. The session starts and stops the channel; the test holds the
// channel to set the sink and run jobs, as a job bridge does.

#pragma once

#include <gtest/gtest.h>

#include <memory>
#include <utility>

#include "tensorplate/core/execution_session.hpp"

#include "adapters/python_pytorch/job_channel.hpp"
#include "adapters/python_pytorch/python_pytorch_session.hpp"
#include "adapters/python_pytorch/sidecar_process.hpp"
#include "job_channel_harness.hpp"

namespace tensorplate::testing {

class JobSessionHarness : public JobBench {
 public:
  JobSessionHarness(adapters::python_pytorch::PythonPytorchConfig config,
                    adapters::python_pytorch::SidecarLauncher launcher) {
    ExecutionSessionRuntimeHooks hooks{};
    hooks.buffer_manager = manager.get();
    auto built = adapters::python_pytorch::make_python_pytorch_session_with_channel(
        hooks, std::move(config), std::move(launcher));
    session = std::move(built.session);
    channel = std::move(built.channel);
    bind(*channel);
    EXPECT_TRUE(channel->set_event_sink(sink).has_value());
  }

  /// Destroys the session, which stops the channel, before the sink is read.
  ~JobSessionHarness() {
    session.reset();
    check_sink();
  }

  JobSessionHarness(const JobSessionHarness&) = delete;
  JobSessionHarness& operator=(const JobSessionHarness&) = delete;
  JobSessionHarness(JobSessionHarness&&) = delete;
  JobSessionHarness& operator=(JobSessionHarness&&) = delete;

  std::unique_ptr<ExecutionSession> session;
  std::shared_ptr<adapters::python_pytorch::JobChannel> channel;
};

}  // namespace tensorplate::testing

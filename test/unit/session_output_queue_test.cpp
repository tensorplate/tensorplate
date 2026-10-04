// SPDX-License-Identifier: Apache-2.0
#include <gtest/gtest.h>

#include <atomic>
#include <chrono>
#include <cstdint>
#include <memory>
#include <optional>
#include <string>
#include <thread>
#include <utility>

#include "fake_scheduler_clock.hpp"
#include "serving/session/output_queue.hpp"
#include "session_budget_fixture.hpp"

namespace tensorplate::serving {
namespace {
using testing::load_session_budget_fixture;
using testing::session_budgets_from;

struct FakeBody final : OutputBody {
  FakeBody(std::string id_in, std::atomic<int>* destroyed_in = nullptr)
      : id(std::move(id_in)), destroyed(destroyed_in) {}
  ~FakeBody() override {
    if (destroyed != nullptr) {
      ++*destroyed;
    }
  }
  std::string id;
  std::atomic<int>* destroyed;
};

std::optional<OutputKind> kind_from(const std::string& name) {
  if (name == "audio") {
    return OutputKind::Audio;
  }
  if (name == "partial") {
    return OutputKind::Partial;
  }
  if (name == "result") {
    return OutputKind::Result;
  }
  if (name == "control") {
    return OutputKind::Control;
  }
  if (name == "terminal") {
    return OutputKind::Terminal;
  }
  return std::nullopt;
}

std::string name_of(OutputOffer offer) {
  switch (offer) {
    case OutputOffer::Queued:
      return "queued";
    case OutputOffer::Replaced:
      return "replaced";
    case OutputOffer::Full:
      return "full";
    case OutputOffer::Suppressed:
      return "suppressed";
    case OutputOffer::Closed:
      return "closed";
  }
  return "unknown";
}

OutputItem make_item(OutputKind kind, std::uint64_t bytes, std::uint64_t key = 0,
                     std::string id = "x") {
  return OutputItem{kind, bytes, key, std::make_unique<FakeBody>(std::move(id))};
}

void run_output_step(BoundedOutputQueue& queue, const nlohmann::json& step,
                     SchedulerClock::TimePoint now) {
  const auto op = step.at("op").get<std::string>();
  if (op == "offer") {
    const auto kind = kind_from(step.at("kind").get<std::string>());
    ASSERT_TRUE(kind);
    auto item = make_item(*kind, step.at("bytes").get<std::uint64_t>(),
                          step.value("key", std::uint64_t{0}), step.at("id").get<std::string>());
    const auto offered = queue.offer(item, now);
    const auto expected = step.at("expect").get<std::string>();
    if (offered) {
      EXPECT_EQ(name_of(*offered), expected);
    } else {
      EXPECT_EQ(offered.error().code, Error::Code::ConfigInvalid);
      EXPECT_EQ(offered.error().context, expected);
    }
    const bool taken =
        offered && (*offered == OutputOffer::Queued || *offered == OutputOffer::Replaced);
    EXPECT_EQ(item.body == nullptr, taken);
  } else if (op == "take") {
    const auto output = queue.take();
    if (step.value("empty", false)) {
      EXPECT_FALSE(output);
      return;
    }
    ASSERT_TRUE(output);
    EXPECT_EQ(output->sequence, step.at("sequence"));
    EXPECT_EQ(queue.last_sequence(), output->sequence);
    const auto* body = dynamic_cast<const FakeBody*>(output->item.body.get());
    ASSERT_NE(body, nullptr);
    EXPECT_EQ(body->id, step.at("id"));
  } else if (op == "delivered") {
    const auto confirmed = queue.delivered(step.at("sequence").get<std::uint64_t>(), now);
    EXPECT_EQ(testing::outcome_of(confirmed), step.at("expect"));
    if (!confirmed) {
      EXPECT_EQ(confirmed.error().code, Error::Code::Internal);
    }
  } else if (op == "suppress") {
    queue.suppress();
  } else {
    FAIL() << "unknown fixture op " << op;
  }
}

TEST(BoundedOutputQueue, CasesMatchFixture) {
  const auto fixture = load_session_budget_fixture();
  const auto& cases = fixture.at("output_cases");
  ASSERT_EQ(cases.size(), 14U);
  std::size_t steps_run = 0;
  for (const auto& test_case : cases) {
    SCOPED_TRACE(test_case.at("name").get<std::string>());
    const auto budgets = session_budgets_from(test_case.at("budget"));
    ASSERT_TRUE(budgets);
    BoundedOutputQueue queue{*budgets};
    EXPECT_FALSE(queue.stalled_since());
    EXPECT_EQ(queue.last_sequence(), 0U);
    const testing::FakeSchedulerClock clock;
    const auto origin = clock.now();
    auto now = origin;
    for (const auto& step : test_case.at("steps")) {
      SCOPED_TRACE(step.dump());
      if (step.contains("at_ms")) {
        now = origin + std::chrono::milliseconds{step.at("at_ms").get<std::int64_t>()};
      }
      run_output_step(queue, step, now);
      if (step.contains("pcm_used")) {
        EXPECT_EQ(queue.pcm_usage().used(), step.at("pcm_used"));
      }
      if (step.contains("metadata_used")) {
        EXPECT_EQ(queue.metadata_usage().used(), step.at("metadata_used"));
      }
      if (step.contains("stalled_since_ms")) {
        const auto& expected = step.at("stalled_since_ms");
        const auto stalled = queue.stalled_since();
        if (expected.is_null()) {
          EXPECT_FALSE(stalled);
        } else {
          ASSERT_TRUE(stalled);
          EXPECT_EQ(*stalled, origin + std::chrono::milliseconds{expected.get<std::int64_t>()});
        }
      }
      EXPECT_EQ(queue.pcm_usage().limit(), budgets->output_pcm_bytes());
      EXPECT_EQ(queue.metadata_usage().limit(), budgets->output_metadata_bytes());
      ++steps_run;
    }
  }
  EXPECT_EQ(steps_run, 145U);
}

TEST(BoundedOutputQueue, ItemWithoutBodyIsRefused) {
  BoundedOutputQueue queue{SessionBudgets::for_text_input(48'000).value()};
  OutputItem item{OutputKind::Control, 4, 0, nullptr};
  const auto offered = queue.offer(item, {});
  ASSERT_FALSE(offered);
  EXPECT_EQ(offered.error().context, "invalid_output_item");
  EXPECT_FALSE(queue.take());
}

TEST(BoundedOutputQueue, ReplacedAndSuppressedBodiesAreDestroyed) {
  std::atomic<int> destroyed{0};
  BoundedOutputQueue queue{SessionBudgets::for_text_input(48'000).value()};
  const auto offer = [&](OutputKind kind, std::uint64_t key) {
    OutputItem item{kind, 10, key, std::make_unique<FakeBody>("x", &destroyed)};
    return queue.offer(item, {}).value();
  };
  EXPECT_EQ(offer(OutputKind::Partial, 1), OutputOffer::Queued);
  EXPECT_EQ(offer(OutputKind::Partial, 1), OutputOffer::Replaced);
  EXPECT_EQ(destroyed.load(), 1);
  EXPECT_EQ(offer(OutputKind::Result, 1), OutputOffer::Queued);
  EXPECT_EQ(destroyed.load(), 2);
  EXPECT_EQ(offer(OutputKind::Audio, 0), OutputOffer::Queued);
  EXPECT_EQ(offer(OutputKind::Control, 0), OutputOffer::Queued);
  queue.suppress();
  EXPECT_EQ(destroyed.load(), 4);
  EXPECT_EQ(queue.pcm_usage().used(), 0U);
  EXPECT_EQ(queue.metadata_usage().used(), 10U);
}

TEST(BoundedOutputQueue, ProducerAndTransportThreadsKeepOrderAndBudget) {
  const auto budgets = SessionBudgets::for_text_input(48'000).value();
  BoundedOutputQueue queue{budgets};
  constexpr std::uint64_t kChunks = 2'000;
  constexpr std::uint64_t kChunkBytes = 960;
  std::atomic<bool> refused_full{false};
  std::atomic<bool> producer_done{false};
  std::atomic<bool> budget_broken{false};
  std::atomic<bool> stop{false};
  std::thread producer([&] {
    for (std::uint64_t chunk = 0; chunk < kChunks && !stop;) {
      auto item = make_item(OutputKind::Audio, kChunkBytes);
      const auto offered = queue.offer(item, {}).value();
      if (offered == OutputOffer::Queued) {
        ++chunk;
      } else {
        refused_full = refused_full || offered == OutputOffer::Full;
        std::this_thread::yield();
      }
      const auto usage = queue.pcm_usage();
      if (usage.limit() != budgets.output_pcm_bytes() || usage.used() > usage.limit()) {
        budget_broken = true;
      }
    }
    producer_done = true;
  });
  // The transport starts only once the producer has run into the budget.
  while (!refused_full && !producer_done) {
    std::this_thread::yield();
  }
  std::uint64_t expected_sequence = 1;
  bool in_order = true;
  while (in_order && expected_sequence <= kChunks) {
    const auto output = queue.take();
    if (!output) {
      std::this_thread::yield();
      continue;
    }
    in_order = output->sequence == expected_sequence &&
               static_cast<bool>(queue.delivered(output->sequence, {}));
    ++expected_sequence;
  }
  stop = !in_order;
  producer.join();
  EXPECT_TRUE(refused_full.load());
  EXPECT_FALSE(budget_broken.load());
  ASSERT_TRUE(in_order);
  EXPECT_EQ(queue.pcm_usage().used(), 0U);
  EXPECT_FALSE(queue.stalled_since());
  EXPECT_FALSE(queue.take());
}
}  // namespace
}  // namespace tensorplate::serving

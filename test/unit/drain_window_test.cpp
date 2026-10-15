// SPDX-License-Identifier: Apache-2.0
//
// The drain window a worker's shutdown shares between its stream sessions and
// its HTTP requests. Every time is given to it: nothing here waits.

#include "serving/drain_window.hpp"

#include <gtest/gtest.h>

#include <chrono>
#include <optional>
#include <thread>
#include <vector>

namespace {

using namespace std::chrono_literals;
using tensorplate::serving::DrainCause;
using tensorplate::serving::DrainWindow;

const DrainWindow::TimePoint kAsked = DrainWindow::TimePoint{} + 1h;

TEST(DrainWindow, TheFirstCallerBeginsItForTheWholeLength) {
  DrainWindow window;
  EXPECT_EQ(window.begin(kAsked, 2s), kAsked + 2s);
}

TEST(DrainWindow, ALaterDrainIsGivenWhatIsLeftOfTheSameWindow) {
  DrainWindow window;
  ASSERT_EQ(window.begin(kAsked, 2s), kAsked + 2s);
  // A drain that starts 1.5 s into the window does not get 2 s of its own.
  EXPECT_EQ(window.begin(kAsked + 1500ms, 2s), kAsked + 2s);
  // One that starts after the window has ended has nothing left.
  EXPECT_EQ(window.begin(kAsked + 10s, 2s), kAsked + 2s);
  EXPECT_EQ(window.begin(kAsked + 10s, 30s), kAsked + 2s);
}

TEST(DrainWindow, AWindowMayBeginAtTheClocksEpoch) {
  DrainWindow window;
  ASSERT_EQ(window.begin(DrainWindow::TimePoint{}, 0ms), DrainWindow::TimePoint{});
  EXPECT_EQ(window.begin(kAsked, 2s), DrainWindow::TimePoint{});
}

TEST(DrainWindow, TheFirstCallerAlsoGivesTheWindowItsCause) {
  DrainWindow shut_down;
  EXPECT_EQ(shut_down.cause(), DrainCause::WorkerShutdown) << "before the window begins";
  ASSERT_EQ(shut_down.begin(kAsked, 2s), kAsked + 2s);
  EXPECT_EQ(shut_down.begin(kAsked + 1s, 30s, DrainCause::DeploymentRetired), kAsked + 2s);
  EXPECT_EQ(shut_down.cause(), DrainCause::WorkerShutdown);

  DrainWindow retired;
  ASSERT_EQ(retired.begin(kAsked, 30s, DrainCause::DeploymentRetired), kAsked + 30s);
  EXPECT_EQ(retired.begin(kAsked + 1s, 2s), kAsked + 30s);
  EXPECT_EQ(retired.cause(), DrainCause::DeploymentRetired);
}

TEST(DrainWindow, AShortenEndsABegunWindowEarlierAndKeepsItsCause) {
  DrainWindow window;
  EXPECT_EQ(window.end(), DrainWindow::TimePoint::min()) << "before the window begins";
  ASSERT_EQ(window.begin(kAsked, 30s, DrainCause::DeploymentRetired), kAsked + 30s);
  EXPECT_EQ(window.end(), kAsked + 30s);
  EXPECT_EQ(window.shorten(kAsked + 10s, 5s), kAsked + 15s);
  EXPECT_EQ(window.end(), kAsked + 15s);
  EXPECT_EQ(window.begin(kAsked + 11s, 30s), kAsked + 15s);
  EXPECT_EQ(window.cause(), DrainCause::DeploymentRetired);
}

TEST(DrainWindow, AShortenNeitherLengthensAWindowNorBeginsOne) {
  DrainWindow window;
  EXPECT_EQ(window.shorten(kAsked, 5s), std::nullopt);
  EXPECT_EQ(window.begin(kAsked + 1s, 2s), kAsked + 3s) << "the shorten began nothing";
  EXPECT_EQ(window.shorten(kAsked + 2s, 5s), std::nullopt);
  EXPECT_EQ(window.shorten(kAsked + 1s, 2s), std::nullopt) << "an end it already has";
  EXPECT_EQ(window.end(), kAsked + 3s);
}

TEST(DrainWindow, CallersThatRaceAreAllGivenOneEndAndOneCause) {
  DrainWindow window;
  constexpr int kCallers = 8;
  // Odd callers retire; each begins at a time of its own, so the end names
  // the caller that began the window.
  const auto cause_of = [](int caller) {
    return caller % 2 == 1 ? DrainCause::DeploymentRetired : DrainCause::WorkerShutdown;
  };
  std::vector<DrainWindow::TimePoint> ends(kCallers);
  std::vector<std::thread> callers;
  for (int caller = 0; caller < kCallers; ++caller) {
    callers.emplace_back(
        [&, caller] { ends[caller] = window.begin(kAsked + caller * 1s, 2s, cause_of(caller)); });
  }
  for (auto& caller : callers) {
    caller.join();
  }
  for (const auto& end : ends) {
    EXPECT_EQ(end, ends.front());
  }
  EXPECT_EQ(window.begin(kAsked + 1min, 2s), ends.front());
  const auto first = static_cast<int>((ends.front() - kAsked - 2s) / 1s);
  EXPECT_EQ(window.cause(), cause_of(first));
}

}  // namespace

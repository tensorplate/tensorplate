// SPDX-License-Identifier: Apache-2.0
//
// The drain window a worker's shutdown shares between its stream sessions and
// its HTTP requests. Every time is given to it: nothing here waits.

#include "serving/drain_window.hpp"

#include <gtest/gtest.h>

#include <chrono>
#include <thread>
#include <vector>

namespace {

using namespace std::chrono_literals;
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

TEST(DrainWindow, CallersThatRaceAreAllGivenOneEnd) {
  DrainWindow window;
  constexpr int kCallers = 8;
  std::vector<DrainWindow::TimePoint> ends(kCallers);
  std::vector<std::thread> callers;
  for (int caller = 0; caller < kCallers; ++caller) {
    callers.emplace_back([&, caller] { ends[caller] = window.begin(kAsked + caller * 1s, 2s); });
  }
  for (auto& caller : callers) {
    caller.join();
  }
  for (const auto& end : ends) {
    EXPECT_EQ(end, ends.front());
  }
  EXPECT_EQ(window.begin(kAsked + 1min, 2s), ends.front());
}

}  // namespace

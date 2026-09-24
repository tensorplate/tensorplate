// SPDX-License-Identifier: Apache-2.0
//
// Holds the scheduler schemas to each other and to the C++ they describe:
// the three schema copies of the scheduler policy enum to each other and to
// the policy registry, and the named gauges of
// protocol/schemas/serving_metrics.json to the open map they belong to.

#include <gtest/gtest.h>

#include <fstream>
#include <nlohmann/json.hpp>
#include <set>
#include <string>

#include "tensorplate/scheduler/factory.hpp"
#include "tensorplate/scheduler/scheduler.hpp"

namespace {

using namespace tensorplate;
using nlohmann::json;

json load(const std::string& relative) {
  std::ifstream in(std::string{TP_SOURCE_DIR} + "/" + relative);
  EXPECT_TRUE(in.is_open()) << relative;
  return json::parse(in);
}

std::set<std::string> keys(const json& object) {
  std::set<std::string> out;
  for (const auto& item : object.items()) {
    out.insert(item.key());
  }
  return out;
}

std::set<std::string> strings(const json& array) {
  std::set<std::string> out;
  for (const auto& item : array) {
    out.insert(item.get<std::string>());
  }
  return out;
}

TEST(SchedulerPolicySchema, EveryCopyListsFifoAndTheReservedKey) {
  const json expected = json::array({"fifo", "session_round_robin"});
  for (const char* path :
       {"config/schemas/scheduler.json", "protocol/schemas/scheduler_metrics.json",
        "protocol/schemas/scheduler_event.json"}) {
    EXPECT_EQ(load(path).at("/properties/policy/enum"_json_pointer), expected) << path;
  }
}

TEST(SchedulerPolicySchema, EveryListedPolicyIsRegisteredOrUnsupported) {
  const auto& registry = SchedulerPolicyRegistry::global();
  const auto listed =
      strings(load("config/schemas/scheduler.json").at("/properties/policy/enum"_json_pointer));
  ASSERT_FALSE(listed.empty());
  for (const auto& policy : listed) {
    SchedulerConfig config;
    config.policy = policy;
    auto built = make_scheduler(config);
    if (registry.is_registered(policy)) {
      ASSERT_TRUE(built) << policy;
      EXPECT_EQ((*built)->policy_name(), policy);
      EXPECT_EQ((*built)->metrics().policy, policy);
    } else {
      ASSERT_FALSE(built) << policy;
      EXPECT_EQ(built.error().code, Error::Code::Unsupported) << policy;
    }
  }
  for (const auto& registered : registry.registered_policies()) {
    EXPECT_TRUE(listed.contains(registered)) << registered << " is registered but not listed";
  }
  EXPECT_TRUE(registry.is_registered("fifo"));
  EXPECT_FALSE(registry.is_registered("session_round_robin"));
}

TEST(SchedulerSchemas, NoSessionFieldInEventsMetricsOrMetricLabels) {
  for (const char* path :
       {"protocol/schemas/scheduler_event.json", "protocol/schemas/scheduler_metrics.json"}) {
    for (const auto& name : keys(load(path).at("properties"))) {
      EXPECT_EQ(name.find("session"), std::string::npos) << path << ": " << name;
    }
  }
  const auto labels =
      load("protocol/schemas/serving_metrics.json").at("/properties/labels"_json_pointer);
  EXPECT_EQ(labels.at("additionalProperties"), false);
  EXPECT_EQ(keys(labels.at("properties")),
            (std::set<std::string>{"endpoint", "model_class", "model_name", "backend"}));
}

TEST(ServingMetricsSchema, NamedGaugesAcceptWhatTheOpenMapAccepts) {
  const auto gauges =
      load("protocol/schemas/serving_metrics.json").at("/properties/gauges"_json_pointer);
  // Any keyword beyond these could constrain the map's keys or values.
  for (const auto& keyword : gauges.items()) {
    EXPECT_TRUE(keyword.key() == "type" || keyword.key() == "additionalProperties" ||
                keyword.key() == "properties")
        << "gauges keyword not understood: " << keyword.key();
  }
  EXPECT_EQ(gauges.at("type"), "object");
  EXPECT_FALSE(gauges.contains("required"));
  ASSERT_FALSE(gauges.at("properties").empty());
  for (const auto& item : gauges.at("properties").items()) {
    auto rule = item.value();
    rule.erase("description");
    EXPECT_EQ(rule, gauges.at("additionalProperties")) << item.key();
  }
}

}  // namespace

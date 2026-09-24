// SPDX-License-Identifier: Apache-2.0
//
// Holds the scheduler's C++ types to the schemas that describe them:
// SchedulerMetrics to protocol/schemas/scheduler_metrics.json, the named
// gauges of protocol/schemas/serving_metrics.json to the open map they belong
// to, and the three schema copies of the scheduler policy enum to each other and to
// the policy registry. Nothing serializes SchedulerMetrics at runtime, so the
// serializer below exists only to compare the struct with its schema.

#include <gtest/gtest.h>

#include <algorithm>
#include <chrono>
#include <cstdint>
#include <fstream>
#include <memory>
#include <nlohmann/json.hpp>
#include <set>
#include <string>
#include <vector>

#include "tensorplate/scheduler/factory.hpp"
#include "tensorplate/scheduler/scheduler.hpp"

#include "fake_scheduler_clock.hpp"
#include "scheduler_fixtures.hpp"

namespace {

using namespace tensorplate;
using namespace tensorplate::testing;
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

// The optional properties of scheduler_metrics.json.
const std::set<std::string> kInFlightSplit{"in_flight_logical", "in_flight_physical",
                                           "in_flight_physical_cancelled",
                                           "in_flight_physical_high_water"};

// The payload scheduler_metrics.json describes for `m`. The structured
// binding names every member, so a member added to or removed from
// SchedulerMetrics stops this file compiling until it is mapped here.
json to_schema_json(const SchedulerMetrics& m) {
  const auto& [policy, queue_depth, queue_depth_high_water, in_flight, in_flight_high_water,
               in_flight_logical, in_flight_physical, in_flight_physical_cancelled,
               in_flight_physical_high_water, admitted_total, admission_rejected_overload,
               admission_rejected_deadline, admission_rejected_pressure, expired_total,
               cancelled_queued, cancelled_in_flight, completed_success, completed_failure,
               pressure_events_memory, pressure_events_thermal, wait_time_sum, wait_time_samples,
               wait_time_max, last_memory_severity, last_thermal_severity] = m;
  const auto ns = [](SchedulerClock::Duration d) {
    return std::chrono::duration_cast<std::chrono::nanoseconds>(d).count();
  };
  return json{
      {"schema_version", "0.1"},
      {"policy", policy},
      {"queue_depth", queue_depth},
      {"queue_depth_high_water", queue_depth_high_water},
      {"in_flight", in_flight},
      {"in_flight_high_water", in_flight_high_water},
      {"in_flight_logical", in_flight_logical},
      {"in_flight_physical", in_flight_physical},
      {"in_flight_physical_cancelled", in_flight_physical_cancelled},
      {"in_flight_physical_high_water", in_flight_physical_high_water},
      {"admitted_total", admitted_total},
      {"admission_rejected_overload", admission_rejected_overload},
      {"admission_rejected_deadline", admission_rejected_deadline},
      {"admission_rejected_pressure", admission_rejected_pressure},
      {"expired_total", expired_total},
      {"cancelled_queued", cancelled_queued},
      {"cancelled_in_flight", cancelled_in_flight},
      {"completed_success", completed_success},
      {"completed_failure", completed_failure},
      {"pressure_events_memory", pressure_events_memory},
      {"pressure_events_thermal", pressure_events_thermal},
      {"wait_time_sum_ns", ns(wait_time_sum)},
      {"wait_time_samples", wait_time_samples},
      {"wait_time_max_ns", ns(wait_time_max)},
      {"last_memory_severity", std::string{to_string(last_memory_severity)}},
      {"last_thermal_severity", std::string{to_string(last_thermal_severity)}},
  };
}

// Checks `doc` against a flat, closed object schema. Only the keywords
// named here are understood, at the top level and in each property rule;
// any other keyword is reported, not skipped.
std::vector<std::string> violations(const json& schema, const json& doc) {
  std::vector<std::string> out;
  const std::set<std::string> understood_top{"$schema",     "$id",       "title",
                                             "description", "type",      "additionalProperties",
                                             "required",    "properties"};
  for (const auto& keyword : schema.items()) {
    if (!understood_top.contains(keyword.key())) {
      out.push_back("schema keyword not understood: " + keyword.key());
    }
  }
  if (schema.at("type") != "object" || schema.at("additionalProperties") != false) {
    out.push_back("schema is not a closed object");
  }
  const std::set<std::string> understood{"type", "minimum", "enum", "const", "description"};
  const auto& properties = schema.at("properties");
  for (const auto& item : doc.items()) {
    const auto& name = item.key();
    const auto& value = item.value();
    if (!properties.contains(name)) {
      out.push_back(name + ": not a declared property");
      continue;
    }
    const auto& rule = properties.at(name);
    for (const auto& keyword : rule.items()) {
      if (!understood.contains(keyword.key())) {
        out.push_back(name + ": keyword not understood: " + keyword.key());
      }
    }
    const auto& type = rule.at("type");
    if (type == "integer") {
      if (!value.is_number_integer() ||
          (rule.contains("minimum") && value.get<std::int64_t>() < rule.at("minimum"))) {
        out.push_back(name + ": not an integer at or above its minimum");
      }
    } else if (type == "string") {
      const bool listed =
          !rule.contains("enum") ||
          std::find(rule.at("enum").begin(), rule.at("enum").end(), value) != rule.at("enum").end();
      const bool constant = !rule.contains("const") || rule.at("const") == value;
      if (!value.is_string() || !listed || !constant) {
        out.push_back(name + ": string outside the declared values");
      }
    } else {
      out.push_back(name + ": type not understood");
    }
  }
  for (const auto& required : schema.at("required")) {
    if (!doc.contains(required.get<std::string>())) {
      out.push_back(required.get<std::string>() + ": required but absent");
    }
  }
  return out;
}

TEST(SchedulerMetricsSchema, NamesEveryMemberAndRequiresAllButTheSplit) {
  const auto schema = load("protocol/schemas/scheduler_metrics.json");
  EXPECT_EQ(schema.at("additionalProperties"), false);
  auto every_member = keys(to_schema_json(SchedulerMetrics{}));
  EXPECT_EQ(keys(schema.at("properties")), every_member);

  for (const auto& name : kInFlightSplit) {
    every_member.erase(name);
  }
  EXPECT_EQ(strings(schema.at("required")), every_member);
}

TEST(SchedulerMetricsSchema, FifoSnapshotsSatisfyTheSchema) {
  const auto schema = load("protocol/schemas/scheduler_metrics.json");
  FakeSchedulerClock clock;
  SchedulerConfig config;
  config.in_flight_capacity = 4;
  SchedulerRuntimeHooks hooks;
  hooks.clock = &clock;
  auto scheduler = make_scheduler(config, hooks).value();

  std::vector<SchedulerMetrics> snapshots{scheduler->metrics()};
  for (const auto* id : {"a", "b", "c", "d", "e"}) {
    ASSERT_TRUE(scheduler->admit(make_scheduler_request(make_infer_request(id), clock)));
  }
  clock.advance(std::chrono::milliseconds{1});
  for (int i = 0; i < 4; ++i) {
    ASSERT_TRUE(scheduler->next().has_value());
  }
  snapshots.push_back(scheduler->metrics());
  ASSERT_TRUE(scheduler->on_completion("d", CompletionStatus::Success));
  ASSERT_TRUE(scheduler->cancel("a", CancellationReason::ClientRequest));
  snapshots.push_back(scheduler->metrics());
  EXPECT_FALSE(scheduler->on_completion("a", CompletionStatus::Success));
  scheduler->on_pressure(
      PressureSignal{PressureSource::Memory, PressureSeverity::Warning, clock.now(), "test"});
  snapshots.push_back(scheduler->metrics());
  EXPECT_EQ(scheduler->shutdown(), 3U);
  snapshots.push_back(scheduler->metrics());

  for (std::size_t i = 0; i < snapshots.size(); ++i) {
    EXPECT_EQ(violations(schema, to_schema_json(snapshots[i])), std::vector<std::string>{})
        << "snapshot " << i;
  }
  // Four dispatched, one completed, one cancelled in flight: the four split
  // counts all differ, so a mapping that swapped or zeroed any of them, or
  // SchedulerMetrics members reordered under the structured binding, would
  // show here.
  const auto cancelled = to_schema_json(snapshots[2]);
  EXPECT_EQ(cancelled.at("in_flight"), 2);
  EXPECT_EQ(cancelled.at("in_flight_logical"), 2);
  EXPECT_EQ(cancelled.at("in_flight_physical"), 3);
  EXPECT_EQ(cancelled.at("in_flight_physical_cancelled"), 1);
  EXPECT_EQ(cancelled.at("in_flight_physical_high_water"), 4);
}

TEST(SchedulerMetricsSchema, SplitCountsAreOptionalAndNeverNullOrNegative) {
  const auto schema = load("protocol/schemas/scheduler_metrics.json");
  SchedulerMetrics m;
  m.policy = "fifo";
  m.in_flight = 1;
  m.in_flight_logical = 1;
  m.in_flight_physical = 2;
  m.in_flight_physical_cancelled = 1;
  m.in_flight_physical_high_water = 2;
  const auto full = to_schema_json(m);
  EXPECT_EQ(violations(schema, full), std::vector<std::string>{});

  // A snapshot written before the split existed still validates.
  auto older = full;
  for (const auto& name : kInFlightSplit) {
    older.erase(name);
  }
  EXPECT_EQ(violations(schema, older), std::vector<std::string>{});

  for (const auto& name : kInFlightSplit) {
    auto doc = full;
    doc[name] = nullptr;
    EXPECT_NE(violations(schema, doc), std::vector<std::string>{}) << name << " = null";
    doc[name] = -1;
    EXPECT_NE(violations(schema, doc), std::vector<std::string>{}) << name << " = -1";
  }
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

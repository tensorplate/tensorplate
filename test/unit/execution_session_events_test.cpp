// SPDX-License-Identifier: Apache-2.0
//
// V01-E04-F06-T01 / T02: Session event-emission tests.
//
// Covers:
//   - paired start/end events for each lifecycle method,
//   - failure events carry `Error::Code` and `backend_name`,
//   - validation rejection emits `validation_failed` events even when
//     no adapter dispatch happens,
//   - the typed-Unsupported async path emits `unsupported_async`,
//     distinct from generic `infer_async_failed`,
//   - event ordering for a load -> prime -> infer -> unload happy path,
//   - a throwing sink cannot corrupt session state (the wrapper
//     swallows the exception and the session lifecycle continues),
//   - events include `model_id` once a model is loaded.

#include <gtest/gtest.h>

#include <chrono>
#include <thread>
#include <utility>
#include <vector>

#include "mock_execution_session.hpp"
#include "recording_event_sink.hpp"
#include "tensorplate/buffer/buffer_ref.hpp"
#include "tensorplate/buffer/tensor_view.hpp"
#include "tensorplate/core/error.hpp"
#include "tensorplate/core/execution_session.hpp"
#include "tensorplate/core/infer_request.hpp"
#include "tensorplate/core/model_spec.hpp"

namespace {

using tensorplate::BufferOwnership;
using tensorplate::BufferRef;
using tensorplate::DType;
using tensorplate::Error;
using tensorplate::InferRequest;
using tensorplate::ModelClass;
using tensorplate::ModelSpec;
using tensorplate::NamedInput;
using tensorplate::NamedOutput;
using tensorplate::SessionEvent;
using tensorplate::SessionEventKind;
using tensorplate::SessionState;
using tensorplate::TensorView;
using tensorplate::testing::MockSession;
using tensorplate::testing::RecordingEventSink;
using tensorplate::testing::ThrowingEventSink;

ModelSpec make_spec() {
  return ModelSpec::create("mock", ModelClass::Vision, "/dev/null", "mock").value();
}

InferRequest valid_request(const std::string& id = "req-1") {
  auto tv = TensorView::create(DType::Float32, {1, 4}).value();
  auto buf = BufferRef::create(1, 16, BufferOwnership::Owned).value();
  return InferRequest::create(id, "/infer", {NamedInput{"in0", buf, tv}}).value();
}

NamedOutput valid_output() {
  auto tv = TensorView::create(DType::Float32, {1, 4}).value();
  auto buf = BufferRef::create(99, 16, BufferOwnership::Owned).value();
  return NamedOutput{"out0", buf, tv, std::nullopt};
}

// -- Lifecycle event ordering -------------------------------------------------

TEST(SessionEvents, FullLifecycleEmitsPairedStartEndEvents) {
  MockSession s;
  RecordingEventSink sink;
  s.set_event_sink(&sink);

  ASSERT_TRUE(s.load(make_spec()).has_value());
  ASSERT_TRUE(s.prime().has_value());
  s.set_next_infer_outputs({valid_output()});
  auto r = s.infer(valid_request());
  ASSERT_TRUE(r.has_value());
  EXPECT_TRUE(r.value().is_success());
  ASSERT_TRUE(s.unload().has_value());

  auto events = sink.events();
  ASSERT_EQ(events.size(), 8u);
  EXPECT_EQ(events[0].kind, SessionEventKind::LoadStart);
  EXPECT_EQ(events[1].kind, SessionEventKind::LoadEnd);
  EXPECT_EQ(events[2].kind, SessionEventKind::PrimeStart);
  EXPECT_EQ(events[3].kind, SessionEventKind::PrimeEnd);
  EXPECT_EQ(events[4].kind, SessionEventKind::InferStart);
  EXPECT_EQ(events[5].kind, SessionEventKind::InferEnd);
  EXPECT_EQ(events[6].kind, SessionEventKind::UnloadStart);
  EXPECT_EQ(events[7].kind, SessionEventKind::UnloadEnd);
}

TEST(SessionEvents, BackendNameIsCarriedOnEveryEvent) {
  MockSession s("mock-vision");
  RecordingEventSink sink;
  s.set_event_sink(&sink);

  ASSERT_TRUE(s.load(make_spec()).has_value());

  for (const auto& ev : sink.events()) {
    EXPECT_EQ(ev.backend_name, "mock-vision");
  }
}

TEST(SessionEvents, ModelIdIsPopulatedAfterLoad) {
  MockSession s;
  RecordingEventSink sink;
  s.set_event_sink(&sink);

  ASSERT_TRUE(s.load(make_spec()).has_value());
  ASSERT_TRUE(s.prime().has_value());

  // First event is LoadStart (model not yet recorded), subsequent
  // events include the loaded model_id.
  auto events = sink.events();
  ASSERT_GE(events.size(), 2u);
  EXPECT_FALSE(events[0].model_id.has_value());  // LoadStart, pre-load.
  EXPECT_EQ(events[0].kind, SessionEventKind::LoadStart);
  EXPECT_TRUE(events[1].model_id.has_value());  // LoadEnd, post-load.
  EXPECT_EQ(*events[1].model_id, "mock");
}

// -- Failure events ----------------------------------------------------------

TEST(SessionEvents, AdapterLoadFailureEmitsLoadFailedWithErrorCode) {
  MockSession s;
  RecordingEventSink sink;
  s.set_event_sink(&sink);
  s.next_load_fails_with(Error::make(Error::Code::LoadFailed, "boom"));

  auto r = s.load(make_spec());
  ASSERT_FALSE(r.has_value());

  auto events = sink.events();
  ASSERT_EQ(events.size(), 2u);
  EXPECT_EQ(events[0].kind, SessionEventKind::LoadStart);
  EXPECT_EQ(events[1].kind, SessionEventKind::LoadFailed);
  ASSERT_TRUE(events[1].error_code.has_value());
  EXPECT_EQ(*events[1].error_code, Error::Code::LoadFailed);
}

TEST(SessionEvents, ValidationFailureBeforeAdapterEmitsValidationFailed) {
  MockSession s;
  RecordingEventSink sink;
  s.set_event_sink(&sink);

  ASSERT_TRUE(s.load(make_spec()).has_value());
  ASSERT_TRUE(s.prime().has_value());
  sink.clear();

  // Tensor window overflows its buffer.
  auto tv = TensorView::create(DType::Float32, {1, 8}).value();
  auto buf = BufferRef::create(1, 16, BufferOwnership::Owned).value();
  auto req =
      InferRequest::create("req-1", "/infer", {NamedInput{"in0", buf, tv}}).value();

  auto r = s.infer(req);
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::ShapeMismatch);

  auto events = sink.events();
  ASSERT_EQ(events.size(), 2u);
  EXPECT_EQ(events[0].kind, SessionEventKind::InferStart);
  EXPECT_EQ(events[1].kind, SessionEventKind::ValidationFailed);
  ASSERT_TRUE(events[1].error_code.has_value());
  EXPECT_EQ(*events[1].error_code, Error::Code::ShapeMismatch);
  ASSERT_TRUE(events[1].request_id.has_value());
  EXPECT_EQ(*events[1].request_id, "req-1");
  EXPECT_EQ(s.dispatch_counts().infer, 0u);
}

TEST(SessionEvents, AsyncUnsupportedEmitsUnsupportedAsyncEvent) {
  MockSession s;
  RecordingEventSink sink;
  s.set_event_sink(&sink);

  ASSERT_TRUE(s.load(make_spec()).has_value());
  ASSERT_TRUE(s.prime().has_value());
  sink.clear();

  auto r = s.infer_async(valid_request());
  ASSERT_FALSE(r.has_value());
  EXPECT_EQ(r.error().code, Error::Code::Unsupported);

  auto events = sink.events();
  ASSERT_EQ(events.size(), 2u);
  EXPECT_EQ(events[0].kind, SessionEventKind::InferAsyncStart);
  EXPECT_EQ(events[1].kind, SessionEventKind::UnsupportedAsync);
  ASSERT_TRUE(events[1].error_code.has_value());
  EXPECT_EQ(*events[1].error_code, Error::Code::Unsupported);
}

TEST(SessionEvents, NativeAsyncFailureEmitsInferAsyncFailedNotUnsupported) {
  MockSession s;
  s.enable_native_async();
  RecordingEventSink sink;
  s.set_event_sink(&sink);

  ASSERT_TRUE(s.load(make_spec()).has_value());
  ASSERT_TRUE(s.prime().has_value());
  s.next_infer_async_fails_with(Error::make(Error::Code::OOMError, "no slot"));
  sink.clear();

  auto r = s.infer_async(valid_request());
  ASSERT_FALSE(r.has_value());

  auto events = sink.events();
  ASSERT_EQ(events.size(), 2u);
  EXPECT_EQ(events[0].kind, SessionEventKind::InferAsyncStart);
  EXPECT_EQ(events[1].kind, SessionEventKind::InferAsyncFailed);
  ASSERT_TRUE(events[1].error_code.has_value());
  EXPECT_EQ(*events[1].error_code, Error::Code::OOMError);
}

TEST(SessionEvents, InferStartCarriesRequestId) {
  MockSession s;
  RecordingEventSink sink;
  s.set_event_sink(&sink);

  ASSERT_TRUE(s.load(make_spec()).has_value());
  ASSERT_TRUE(s.prime().has_value());
  s.set_next_infer_outputs({valid_output()});
  sink.clear();

  auto r = s.infer(valid_request("req-abc"));
  ASSERT_TRUE(r.has_value());

  auto events = sink.events();
  ASSERT_EQ(events.size(), 2u);
  ASSERT_TRUE(events[0].request_id.has_value());
  EXPECT_EQ(*events[0].request_id, "req-abc");
  ASSERT_TRUE(events[1].request_id.has_value());
  EXPECT_EQ(*events[1].request_id, "req-abc");
}

// -- state_after on every event ----------------------------------------------

TEST(SessionEvents, StateAfterFieldReflectsPostEventState) {
  MockSession s;
  RecordingEventSink sink;
  s.set_event_sink(&sink);

  ASSERT_TRUE(s.load(make_spec()).has_value());
  ASSERT_TRUE(s.prime().has_value());

  auto events = sink.events();
  ASSERT_EQ(events.size(), 4u);
  EXPECT_EQ(events[0].state_after, SessionState::Unloaded);  // LoadStart
  EXPECT_EQ(events[1].state_after, SessionState::Loaded);    // LoadEnd
  EXPECT_EQ(events[2].state_after, SessionState::Loaded);    // PrimeStart
  EXPECT_EQ(events[3].state_after, SessionState::Ready);     // PrimeEnd
}

// -- Throwing sink cannot corrupt session state -----------------------------

TEST(SessionEvents, ThrowingSinkDoesNotCorruptSessionState) {
  MockSession s;
  ThrowingEventSink sink;
  s.set_event_sink(&sink);

  // Lifecycle should still complete despite every emit throwing.
  ASSERT_TRUE(s.load(make_spec()).has_value());
  EXPECT_EQ(s.state(), SessionState::Loaded);
  ASSERT_TRUE(s.prime().has_value());
  EXPECT_EQ(s.state(), SessionState::Ready);

  s.set_next_infer_outputs({valid_output()});
  auto r = s.infer(valid_request());
  ASSERT_TRUE(r.has_value());
  EXPECT_TRUE(r.value().is_success());

  ASSERT_TRUE(s.unload().has_value());
  EXPECT_EQ(s.state(), SessionState::Unloaded);

  EXPECT_GT(sink.calls(), 0u);
}

// -- Setting a null sink is a safe no-op ------------------------------------

TEST(SessionEvents, NullSinkIsSafeAndProducesNoEvents) {
  MockSession s;
  // Default: no sink wired.
  ASSERT_TRUE(s.load(make_spec()).has_value());
  ASSERT_TRUE(s.prime().has_value());
  s.set_next_infer_outputs({valid_output()});
  auto r = s.infer(valid_request());
  EXPECT_TRUE(r.has_value());
}

// -- Event duration is non-negative -----------------------------------------

TEST(SessionEvents, AdapterEventDurationIsNonNegative) {
  MockSession s;
  RecordingEventSink sink;
  s.set_event_sink(&sink);

  ASSERT_TRUE(s.load(make_spec()).has_value());
  ASSERT_TRUE(s.prime().has_value());
  s.set_next_infer_outputs({valid_output()});
  sink.clear();
  ASSERT_TRUE(s.infer(valid_request()).has_value());

  auto events = sink.events();
  ASSERT_EQ(events.size(), 2u);
  EXPECT_EQ(events[0].duration.count(), 0);  // InferStart carries zero.
  EXPECT_GE(events[1].duration.count(), 0);  // InferEnd carries adapter duration.
}

}  // namespace

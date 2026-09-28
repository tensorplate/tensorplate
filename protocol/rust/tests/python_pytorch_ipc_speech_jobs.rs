// SPDX-License-Identifier: Apache-2.0
//
// The sidecar's job and session messages against their golden frames, the
// schema and the typed job seam.
//
// `tests/fixtures/python_pytorch_ipc_speech_jobs_*.jsonl` hold one header per
// line. The trace files are the job seam's traces
// (`protocol/fixtures/job_seam.json`) carried over the socket, and this suite
// renders them again to prove it. It also replays every job seam vector whose
// scope is `all` through the schema and the Rust mirror.

#![allow(clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::OnceLock;

use serde_json::{json, Map, Value};
use tensorplate_protocol::python_pytorch_ipc::{
    CAPABILITY_SPEECH_JOBS_V1, JOB_INPUT_SAMPLE_RATE_HZ, JOB_OUTPUT_SAMPLE_RATE_HZ,
    MAX_AUDIO_CHUNK_BYTES, MAX_AUDIO_FRAMES_BYTES, MAX_JOB_ERROR_TEXT_BYTES,
    MAX_JOB_PROGRESS_EVENTS, MAX_LANGUAGE_TAG_BYTES, MAX_TEXT_SEGMENT_BYTES,
    MAX_TRANSCRIPT_TEXT_BYTES, MAX_TRANSCRIPT_TOKENS, MAX_VAD_FRAMES_BYTES, MAX_VAD_FRAMES_PER_JOB,
    MAX_VAD_PROBABILITIES, MAX_VOICE_ID_BYTES,
};
use tensorplate_protocol::{
    decode_with_version_check, IpcJobClass, IpcMessage, IpcMessageError, IpcMessageKind,
};

/// Golden files, each with the job seam trace it carries, if any.
const GOLDEN: [(&str, Option<&str>); 9] = [
    ("stt_decode", Some("speech_stt_decode")),
    ("tts_synthesis", Some("speech_tts_synthesis")),
    ("vad_frames", Some("speech_vad_frames")),
    ("cancel", Some("cancel_while_running")),
    ("refused", Some("failed_before_acceptance")),
    (
        "interleaved_progress",
        Some("delegated_interleaved_progress"),
    ),
    ("negotiation", None),
    ("negotiation_refused", None),
    ("session_release", None),
];

/// Reasons the receiver checks on the payload bytes, which no header carries.
const PAYLOAD_REASONS: [&str; 1] = ["text_invalid_utf8"];
/// Reasons a closed enum of the mirror refuses while decoding.
const DECODE_REASONS: [&str; 2] = ["job_class_unknown", "audio_format_unsupported"];
/// Reasons Draft-07 cannot state (string byte counts, relations between
/// fields); the mirror checks them, the schema may not.
const MIRROR_ONLY_REASONS: [&str; 9] = [
    "text_too_large",
    "error_text_too_large",
    "vad_frame_bytes_mismatch",
    "word_token_interval_empty",
    "word_token_interval_out_of_range",
    "word_token_interval_overlap",
    "word_sample_interval_reversed",
    "word_sample_order",
    "clipped_samples_out_of_range",
];
/// Reasons of vectors JSON cannot carry: ill-formed UTF-8 in a header string,
/// and NaN or infinite probabilities.
const UNREPRESENTABLE_REASONS: [&str; 2] = ["text_invalid_utf8", "vad_probability_out_of_range"];

fn repo_path(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative)
}

fn read_json(relative: &str) -> Value {
    let path = repo_path(relative);
    let raw =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

fn schema() -> &'static Value {
    static SCHEMA: OnceLock<Value> = OnceLock::new();
    SCHEMA.get_or_init(|| read_json("protocol/schemas/python_pytorch_ipc.json"))
}

fn seam() -> &'static Value {
    static SEAM: OnceLock<Value> = OnceLock::new();
    SEAM.get_or_init(|| read_json("protocol/fixtures/job_seam.json"))
}

fn validator() -> &'static jsonschema::JSONSchema {
    static VALIDATOR: OnceLock<jsonschema::JSONSchema> = OnceLock::new();
    VALIDATOR.get_or_init(|| {
        let mut options = jsonschema::JSONSchema::options();
        for referenced in ["error.json", "model_spec.json", "tensor_view.json"] {
            let document = read_json(&format!("protocol/schemas/{referenced}"));
            let id = document["$id"].as_str().expect("$id").to_owned();
            options.with_document(id, document);
        }
        options
            .compile(schema())
            .expect("schema compiles as Draft-07")
    })
}

fn golden_lines(file: &str) -> Vec<String> {
    let path = repo_path(&format!(
        "protocol/rust/tests/fixtures/python_pytorch_ipc_speech_jobs_{file}.jsonl"
    ));
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert!(text.ends_with('\n'), "{file}: last frame lacks its newline");
    text.lines().map(str::to_owned).collect()
}

/// Decodes and validates `frame` as the mirror does.
fn mirror(frame: &Value) -> Result<IpcMessage, String> {
    let message: IpcMessage = serde_json::from_value(frame.clone()).map_err(|e| e.to_string())?;
    message.validate().map_err(|e| match e {
        IpcMessageError::InvalidJob(reason) => reason.to_owned(),
        other => other.to_string(),
    })
}

fn schema_accepts(frame: &Value) -> bool {
    validator().is_valid(frame)
}

// -- Rendering job seam vectors as socket headers. ----------------------------

fn text_bytes(text: &Value) -> Vec<u8> {
    if let Some(utf8) = text.get("utf8") {
        return utf8.as_str().expect("utf8").as_bytes().to_vec();
    }
    if let Some(hex) = text.get("hex") {
        let hex = hex.as_str().expect("hex");
        return (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex byte"))
            .collect();
    }
    let unit = text["repeat"].as_str().expect("repeat");
    let times = usize::try_from(text["times"].as_u64().expect("times")).expect("times fits");
    let mut bytes = unit.repeat(times).into_bytes();
    if let Some(tail) = text.get("then") {
        bytes.extend_from_slice(tail.as_str().expect("then").as_bytes());
    }
    bytes
}

/// `None` when the text is not well-formed UTF-8, which no JSON string holds.
fn text_string(text: &Value) -> Option<Value> {
    String::from_utf8(text_bytes(text)).ok().map(Value::String)
}

fn audio_format(format: &Value) -> Value {
    match format.as_str() {
        Some(name) => seam()["formats"][name].clone(),
        None => format.clone(),
    }
}

fn pcm_size(pcm: &Value) -> Value {
    pcm["size"].clone()
}

fn render_input(payload: &Value) -> Value {
    match payload["type"].as_str().expect("payload type") {
        "audio_frames" => json!({
            "type": "audio_frames",
            "format": audio_format(&payload["format"]),
            "start_sample": payload["start_sample"],
            "payload_length": pcm_size(&payload["pcm"]),
        }),
        "text_segment" => json!({
            "type": "text_segment",
            "payload_length": text_bytes(&payload["text"]).len(),
        }),
        "vad_frames" => json!({
            "type": "vad_frames",
            "format": audio_format(&payload["format"]),
            "frame_samples": payload["frame_samples"],
            "frame_count": payload["frame_count"],
            "utterance_id": payload["utterance_id"],
            "payload_length": pcm_size(&payload["pcm"]),
        }),
        other => panic!("unknown payload type {other}"),
    }
}

fn envelope(message_id: &str, kind: &str, identity: &Value) -> Map<String, Value> {
    let mut frame = Map::new();
    frame.insert("schema_version".into(), json!("0.1"));
    frame.insert("message_id".into(), json!(message_id));
    frame.insert("kind".into(), json!(kind));
    for field in ["job_id", "session_key", "generation"] {
        frame.insert(field.into(), identity[field].clone());
    }
    frame
}

fn render_request(message_id: &str, request: &Value) -> Value {
    let mut frame = envelope(message_id, "job_submit", &request["identity"]);
    frame.insert("job_class".into(), request["job_class"].clone());
    frame.insert("progress_limit".into(), request["progress_limit"].clone());
    frame.insert("options".into(), request["options"].clone());
    frame.insert("input".into(), render_input(&request["payload"]));
    Value::Object(frame)
}

fn render_tokens(tokens: &Value) -> Value {
    match tokens.get("count") {
        Some(count) => (1..=count.as_u64().expect("count")).collect(),
        None => tokens.clone(),
    }
}

fn render_words(words: &Value, token_count: usize) -> Option<Value> {
    if let Some(template) = words.get("one_per_token") {
        let text = text_string(&template["text"])?;
        let units = (0..token_count as u64).map(|i| {
            json!({
                "text": text,
                "token_begin": i,
                "token_end": i + 1,
                "start_sample": 512 * i,
                "end_sample": 512 * (i + 1),
            })
        });
        return Some(units.collect());
    }
    let mut units = Vec::new();
    for word in words.as_array().expect("words") {
        let mut unit = word.as_object().expect("word").clone();
        unit.insert("text".into(), text_string(&word["text"])?);
        units.push(Value::Object(unit));
    }
    Some(Value::Array(units))
}

/// `None` when JSON cannot carry the result.
fn render_result(result: &Value) -> Option<Value> {
    match result["type"].as_str().expect("result type") {
        "transcript" => {
            let tokens = render_tokens(&result["tokens"]);
            let token_count = tokens.as_array().expect("tokens").len();
            Some(json!({
                "type": "transcript",
                "text": text_string(&result["text"])?,
                "tokens": tokens,
                "words": render_words(&result["words"], token_count)?,
            }))
        }
        "audio_chunk" => Some(json!({
            "type": "audio_chunk",
            "format": audio_format(&result["format"]),
            "clipped_samples": result["clipped_samples"],
            "payload_length": pcm_size(&result["pcm"]),
        })),
        "vad" => {
            let probabilities = result["probabilities"].as_array().expect("probabilities");
            if probabilities.iter().any(Value::is_string) {
                return None;
            }
            Some(json!({"type": "vad", "probabilities": probabilities}))
        }
        other => panic!("unknown result type {other}"),
    }
}

/// `None` when JSON cannot carry the event.
fn render_event(message_id: &str, event: &Value, identity: &Value) -> Option<Value> {
    let kind = event["kind"].as_str().expect("event kind");
    let identity = event.get("identity").unwrap_or(identity);
    let mut frame = envelope(message_id, &format!("job_{kind}"), identity);
    if let Some(error) = event.get("error") {
        let mut wire = Map::new();
        wire.insert("schema_version".into(), json!("0.1"));
        wire.insert("code".into(), error["code"].clone());
        wire.insert("message".into(), text_string(&error["message"])?);
        if let Some(context) = error.get("context") {
            wire.insert("context".into(), text_string(context)?);
        }
        frame.insert("error".into(), Value::Object(wire));
    }
    if let Some(sequence) = event.get("progress_sequence") {
        frame.insert("progress_sequence".into(), sequence.clone());
    }
    if let Some(result) = event.get("result") {
        frame.insert("result".into(), render_result(result)?);
    }
    Some(Value::Object(frame))
}

/// A trace on the socket: every job submitted in order, then each step as the
/// sidecar's event or the adapter's `job_cancel`.
fn render_trace(trace: &Value) -> Vec<Value> {
    let jobs = trace["jobs"].as_object().expect("jobs");
    let (mut adapter, mut sidecar) = (0, 0);
    let mut frames = Vec::new();
    for job in jobs.values() {
        adapter += 1;
        frames.push(render_request(&format!("a{adapter}"), job));
    }
    for step in trace["steps"].as_array().expect("steps") {
        let identity = &jobs[step["job"].as_str().expect("job")]["identity"];
        if step.get("cancel").is_some() {
            adapter += 1;
            let frame = envelope(&format!("a{adapter}"), "job_cancel", identity);
            frames.push(Value::Object(frame));
        } else {
            sidecar += 1;
            let frame = render_event(&format!("s{sidecar}"), step, identity)
                .expect("trace steps are representable");
            frames.push(frame);
        }
    }
    frames
}

fn seam_trace(name: &str) -> &'static Value {
    seam()["traces"]
        .as_array()
        .expect("traces")
        .iter()
        .find(|trace| trace["name"] == name)
        .unwrap_or_else(|| panic!("no job seam trace named {name}"))
}

fn in_scope_all(vector: &Value) -> bool {
    vector.get("scope").map_or(true, |scope| scope == "all")
}

// -- Golden frames. -----------------------------------------------------------

#[test]
fn golden_frames_round_trip_byte_for_byte() {
    for (file, _) in GOLDEN {
        for (index, line) in golden_lines(file).iter().enumerate() {
            let message: IpcMessage = decode_with_version_check(line)
                .unwrap_or_else(|e| panic!("{file}:{}: {e}", index + 1));
            let bytes = serde_json::to_string(&message).expect("encode");
            assert_eq!(&bytes, line, "{file}:{} re-encodes differently", index + 1);
        }
    }
}

#[test]
fn golden_frames_satisfy_the_schema() {
    for (file, _) in GOLDEN {
        for (index, line) in golden_lines(file).iter().enumerate() {
            let frame: Value = serde_json::from_str(line).expect("json");
            assert!(
                schema_accepts(&frame),
                "{file}:{} fails the schema",
                index + 1
            );
        }
    }
}

#[test]
fn golden_traces_are_the_job_seam_traces_on_the_socket() {
    for (file, trace) in GOLDEN {
        let Some(trace) = trace else { continue };
        let golden: Vec<Value> = golden_lines(file)
            .iter()
            .map(|line| serde_json::from_str(line).expect("json"))
            .collect();
        assert_eq!(
            golden,
            render_trace(seam_trace(trace)),
            "{file} is not the trace {trace}"
        );
    }
}

#[test]
fn golden_negotiation_enables_the_job_messages() {
    let frames: Vec<IpcMessage> = golden_lines("negotiation")
        .iter()
        .map(|line| decode_with_version_check(line).expect("frame"))
        .collect();
    let enabled = Some(vec![CAPABILITY_SPEECH_JOBS_V1.to_owned()]);
    assert_eq!(frames[0].kind, IpcMessageKind::ReadyEvent);
    assert_eq!(frames[0].capabilities, enabled);
    assert_eq!(frames[1].kind, IpcMessageKind::LoadModel);
    assert_eq!(frames[1].capabilities, enabled);
    assert_eq!(frames[2].message_id, frames[1].message_id);
    assert_eq!(
        frames[2].job_classes,
        Some(vec![IpcJobClass::SttDecode, IpcJobClass::VadFrames])
    );
}

#[test]
fn every_golden_file_on_disk_is_checked() {
    let dir = repo_path("protocol/rust/tests/fixtures");
    let mut on_disk: Vec<String> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter_map(|name| {
            name.strip_prefix("python_pytorch_ipc_speech_jobs_")
                .and_then(|rest| rest.strip_suffix(".jsonl"))
                .map(str::to_owned)
        })
        .collect();
    on_disk.sort();
    let mut listed: Vec<String> = GOLDEN.iter().map(|(file, _)| (*file).to_owned()).collect();
    listed.sort();
    assert_eq!(on_disk, listed);
}

// -- Names and bounds in step with the job seam. ------------------------------

fn wire_names(values: &Value) -> BTreeSet<String> {
    values
        .as_array()
        .expect("enum")
        .iter()
        .map(|value| value.as_str().expect("name").to_owned())
        .collect()
}

/// Every kind. Adding an `IpcMessageKind` variant fails to compile here until
/// it is listed, and the schema comparison fails until the schema lists it.
fn every_kind() -> Vec<IpcMessageKind> {
    use IpcMessageKind as K;
    let every = [
        K::LoadModel,
        K::LoadModelResponse,
        K::Prime,
        K::PrimeResponse,
        K::Infer,
        K::InferResponse,
        K::InferAsync,
        K::InferAsyncResponse,
        K::Cancel,
        K::CancelResponse,
        K::Unload,
        K::UnloadResponse,
        K::HealthCheck,
        K::HealthCheckResponse,
        K::ReadyEvent,
        K::ErrorEvent,
        K::MetricEvent,
        K::JobSubmit,
        K::JobCancel,
        K::JobAccepted,
        K::JobProgress,
        K::JobCompleted,
        K::JobFailed,
        K::JobCancelAcknowledged,
        K::JobReleased,
        K::SessionRelease,
        K::SessionReleased,
    ];
    for kind in every {
        match kind {
            K::LoadModel
            | K::LoadModelResponse
            | K::Prime
            | K::PrimeResponse
            | K::Infer
            | K::InferResponse
            | K::InferAsync
            | K::InferAsyncResponse
            | K::Cancel
            | K::CancelResponse
            | K::Unload
            | K::UnloadResponse
            | K::HealthCheck
            | K::HealthCheckResponse
            | K::ReadyEvent
            | K::ErrorEvent
            | K::MetricEvent
            | K::JobSubmit
            | K::JobCancel
            | K::JobAccepted
            | K::JobProgress
            | K::JobCompleted
            | K::JobFailed
            | K::JobCancelAcknowledged
            | K::JobReleased
            | K::SessionRelease
            | K::SessionReleased => {}
        }
    }
    every.to_vec()
}

#[test]
fn schema_kinds_match_the_mirror() {
    let schema_kinds: Vec<String> = schema()["properties"]["kind"]["enum"]
        .as_array()
        .expect("kinds")
        .iter()
        .map(|kind| kind.as_str().expect("kind").to_owned())
        .collect();
    let mirror_kinds: Vec<String> = every_kind()
        .into_iter()
        .map(|kind| {
            let wire = serde_json::to_value(kind).expect("kind");
            assert_eq!(wire, kind.as_str(), "as_str disagrees with serde");
            kind.as_str().to_owned()
        })
        .collect();
    assert_eq!(schema_kinds, mirror_kinds);

    let definitions = &schema()["definitions"];
    let job: BTreeSet<String> = every_kind()
        .into_iter()
        .filter(|kind| kind.is_job())
        .map(|kind| kind.as_str().to_owned())
        .collect();
    let session: BTreeSet<String> = every_kind()
        .into_iter()
        .filter(|kind| kind.is_session())
        .map(|kind| kind.as_str().to_owned())
        .collect();
    assert_eq!(wire_names(&definitions["JobMessageKind"]["enum"]), job);
    assert_eq!(
        wire_names(&definitions["SessionMessageKind"]["enum"]),
        session
    );
}

#[test]
fn job_names_match_the_job_seam() {
    let names = &seam()["names"];
    let definitions = &schema()["definitions"];
    let classes = wire_names(&names["job_class"]);
    assert_eq!(wire_names(&definitions["JobClass"]["enum"]), classes);
    for class in [
        IpcJobClass::SttDecode,
        IpcJobClass::TtsSynthesis,
        IpcJobClass::VadFrames,
    ] {
        let wire = serde_json::to_value(class).expect("class");
        assert!(classes.contains(wire.as_str().expect("class")));
    }
    assert_eq!(
        wire_names(&definitions["JobResult"]["properties"]["type"]["enum"]),
        wire_names(&names["result_kind"])
    );
    let encodings = wire_names(&names["audio_encoding"]);
    for format in ["JobInputAudioFormat", "JobOutputAudioFormat"] {
        let encoding = &definitions[format]["properties"]["encoding"]["enum"];
        assert_eq!(wire_names(encoding), encodings, "{format}");
    }
    // The sidecar's events are the seam's event kinds; the adapter sends the
    // other two job kinds.
    let events: BTreeSet<String> = wire_names(&names["job_event_kind"])
        .into_iter()
        .map(|kind| format!("job_{kind}"))
        .collect();
    let mut expected = wire_names(&definitions["JobMessageKind"]["enum"]);
    expected.remove("job_submit");
    expected.remove("job_cancel");
    assert_eq!(events, expected);
}

fn limit(name: &str) -> u64 {
    seam()["limits"][name]
        .as_u64()
        .unwrap_or_else(|| panic!("job seam limit {name}"))
}

fn usize_limit(name: &str) -> usize {
    usize::try_from(limit(name)).expect("limit fits usize")
}

/// The `then.properties.payload_length.maximum` of the `allOf` branch whose
/// `if` names `type`.
fn branch_maximum(definition: &Value, type_name: &str) -> u64 {
    definition["allOf"]
        .as_array()
        .expect("allOf")
        .iter()
        .find(|branch| branch["if"]["properties"]["type"]["const"] == type_name)
        .and_then(|branch| branch["then"]["properties"]["payload_length"]["maximum"].as_u64())
        .unwrap_or_else(|| panic!("no payload_length maximum for {type_name}"))
}

fn failed_error_maxima() -> (u64, u64) {
    let branch = schema()["allOf"]
        .as_array()
        .expect("allOf")
        .iter()
        .find(|branch| branch["if"]["properties"]["kind"]["const"] == "job_failed")
        .expect("job_failed rule");
    let error = &branch["then"]["properties"]["error"]["properties"];
    (
        error["message"]["maxLength"].as_u64().expect("message"),
        error["context"]["maxLength"].as_u64().expect("context"),
    )
}

#[test]
fn schema_and_mirror_bounds_are_the_job_seam_limits() {
    let properties = &schema()["properties"];
    let definitions = &schema()["definitions"];
    let input = &definitions["JobInput"];
    let result = &definitions["JobResult"]["properties"];
    let options = &definitions["JobOptions"]["properties"];
    let rate = |format: &str| &definitions[format]["properties"]["sample_rate_hz"]["enum"][0];

    let progress = limit("progress_events_max");
    assert_eq!(properties["progress_limit"]["maximum"], progress);
    assert_eq!(properties["progress_sequence"]["maximum"], progress);
    assert_eq!(u64::from(MAX_JOB_PROGRESS_EVENTS), progress);

    let input_rate = limit("job_input_sample_rate_hz");
    assert_eq!(*rate("JobInputAudioFormat"), input_rate);
    assert_eq!(u64::from(JOB_INPUT_SAMPLE_RATE_HZ), input_rate);
    let output_rate = limit("job_output_sample_rate_hz");
    assert_eq!(*rate("JobOutputAudioFormat"), output_rate);
    assert_eq!(u64::from(JOB_OUTPUT_SAMPLE_RATE_HZ), output_rate);

    let audio = limit("audio_frames_max_bytes");
    assert_eq!(branch_maximum(input, "audio_frames"), audio);
    assert_eq!(MAX_AUDIO_FRAMES_BYTES, audio);
    let text = limit("text_segment_max_bytes");
    assert_eq!(branch_maximum(input, "text_segment"), text);
    assert_eq!(MAX_TEXT_SEGMENT_BYTES, text);
    let vad_bytes = limit("vad_frames_max_bytes");
    assert_eq!(branch_maximum(input, "vad_frames"), vad_bytes);
    assert_eq!(MAX_VAD_FRAMES_BYTES, vad_bytes);
    let vad_frames = limit("vad_frames_max_per_job");
    assert_eq!(input["properties"]["frame_count"]["maximum"], vad_frames);
    assert_eq!(u64::from(MAX_VAD_FRAMES_PER_JOB), vad_frames);

    assert_eq!(
        options["language"]["maxLength"],
        limit("language_tag_max_bytes")
    );
    assert_eq!(
        MAX_LANGUAGE_TAG_BYTES,
        usize_limit("language_tag_max_bytes")
    );
    assert_eq!(options["voice"]["maxLength"], limit("voice_id_max_bytes"));
    assert_eq!(MAX_VOICE_ID_BYTES, usize_limit("voice_id_max_bytes"));

    let transcript = limit("transcript_text_max_bytes");
    assert_eq!(result["text"]["maxLength"], transcript);
    assert_eq!(
        MAX_TRANSCRIPT_TEXT_BYTES,
        usize_limit("transcript_text_max_bytes")
    );
    let tokens = limit("transcript_tokens_max");
    assert_eq!(result["tokens"]["maxItems"], tokens);
    assert_eq!(result["words"]["maxItems"], tokens);
    assert_eq!(MAX_TRANSCRIPT_TOKENS, usize_limit("transcript_tokens_max"));
    let chunk = limit("audio_chunk_max_bytes");
    assert_eq!(result["payload_length"]["maximum"], chunk);
    assert_eq!(result["clipped_samples"]["maximum"], chunk / 2);
    assert_eq!(MAX_AUDIO_CHUNK_BYTES, chunk);
    let probabilities = limit("vad_probabilities_max");
    assert_eq!(result["probabilities"]["maxItems"], probabilities);
    assert_eq!(MAX_VAD_PROBABILITIES, usize_limit("vad_probabilities_max"));

    let error_text = limit("error_text_max_bytes");
    assert_eq!(failed_error_maxima(), (error_text, error_text));
    assert_eq!(
        MAX_JOB_ERROR_TEXT_BYTES,
        usize_limit("error_text_max_bytes")
    );
}

// -- Replaying the job seam vectors. -------------------------------------------

/// Checks one rendered vector against its expectation: an accepted vector
/// passes the schema and the mirror; a refused one fails the mirror with the
/// seam's reason, or is one of the named classes the header cannot check.
fn replay(name: &str, frame: Option<Value>, expect: &Value) {
    let Some(reason) = expect["reject"].as_str() else {
        let frame = frame.unwrap_or_else(|| panic!("{name}: accepted vector not representable"));
        assert!(schema_accepts(&frame), "{name}: schema refuses {frame}");
        if let Err(e) = mirror(&frame) {
            panic!("{name}: mirror refuses: {e}");
        }
        return;
    };
    let Some(frame) = frame else {
        assert!(
            UNREPRESENTABLE_REASONS.contains(&reason),
            "{name}: unrepresentable for {reason}"
        );
        return;
    };
    match mirror(&frame) {
        Ok(_) => {
            assert!(
                PAYLOAD_REASONS.contains(&reason),
                "{name}: mirror accepts a vector refused with {reason}"
            );
            assert!(
                schema_accepts(&frame),
                "{name}: schema refuses a valid header"
            );
        }
        Err(got) if got == reason => {
            if !MIRROR_ONLY_REASONS.contains(&reason) {
                assert!(!schema_accepts(&frame), "{name}: schema accepts {reason}");
            }
        }
        Err(got) => {
            let decoded = serde_json::from_value::<IpcMessage>(frame.clone()).is_ok();
            assert!(
                !decoded && DECODE_REASONS.contains(&reason),
                "{name}: mirror refuses with `{got}`, the seam with `{reason}`"
            );
            assert!(!schema_accepts(&frame), "{name}: schema accepts {reason}");
        }
    }
}

fn vectors(section: &str) -> impl Iterator<Item = &'static Value> {
    seam()[section]
        .as_array()
        .expect("vectors")
        .iter()
        .filter(|vector| in_scope_all(vector))
}

#[test]
fn every_request_vector_replays_on_the_socket() {
    let mut count = 0;
    for vector in vectors("requests") {
        let name = vector["name"].as_str().expect("name");
        let frame = render_request("a1", &vector["request"]);
        replay(name, Some(frame), &vector["expect"]);
        count += 1;
    }
    assert_eq!(count, 78, "scope-all request vectors");
}

#[test]
fn every_result_vector_replays_on_the_socket() {
    let identity = json!({"job_id": 1, "session_key": 1, "generation": 1});
    let mut count = 0;
    for vector in vectors("results") {
        let name = vector["name"].as_str().expect("name");
        let event = json!({"kind": "completed", "result": vector["result"]});
        let frame = render_event("s1", &event, &identity);
        replay(name, frame, &vector["expect"]);
        count += 1;
    }
    assert_eq!(count, 35, "scope-all result vectors");
}

#[test]
fn every_event_vector_replays_on_the_socket() {
    let mut count = 0;
    for vector in vectors("events") {
        let name = vector["name"].as_str().expect("name");
        let event = &vector["event"];
        let frame = render_event("s1", event, &event["identity"]);
        replay(name, frame, &vector["expect"]);
        count += 1;
    }
    assert_eq!(count, 12, "scope-all event vectors");
}

/// A refused trace breaks a sequence rule, which the receiver checks across
/// messages; every message of every trace is itself a valid header.
#[test]
fn every_trace_message_is_a_valid_header() {
    let mut count = 0;
    for trace in seam()["traces"].as_array().expect("traces") {
        let name = trace["name"].as_str().expect("name");
        for frame in render_trace(trace) {
            assert!(schema_accepts(&frame), "{name}: schema refuses {frame}");
            if let Err(e) = mirror(&frame) {
                panic!("{name}: mirror refuses {frame}: {e}");
            }
            count += 1;
        }
    }
    assert_eq!(count, 207, "trace messages");
}

#[test]
fn the_classified_reasons_exist_in_the_job_seam() {
    let reasons = wire_names(&seam()["reasons"]["construction"]);
    for reason in PAYLOAD_REASONS
        .iter()
        .chain(&DECODE_REASONS)
        .chain(&MIRROR_ONLY_REASONS)
        .chain(&UNREPRESENTABLE_REASONS)
    {
        assert!(reasons.contains(*reason), "{reason} is not a seam reason");
    }
}

// -- Single faults. --------------------------------------------------------------

fn golden_frame(file: &str, line: usize) -> Value {
    serde_json::from_str(&golden_lines(file)[line]).expect("json")
}

fn with(mut frame: Value, pointer: &str, value: Value) -> Value {
    let (parent, key) = pointer.rsplit_once('/').expect("pointer");
    let target = frame
        .pointer_mut(parent)
        .and_then(Value::as_object_mut)
        .unwrap_or_else(|| panic!("no object at {parent}"));
    if value.is_null() && key.starts_with('-') {
        target.remove(&key[1..]);
    } else {
        target.insert(key.into(), value);
    }
    frame
}

fn without(frame: Value, pointer: &str) -> Value {
    let (parent, key) = pointer.rsplit_once('/').expect("pointer");
    with(frame, &format!("{parent}/-{key}"), Value::Null)
}

fn health_check() -> Value {
    json!({"schema_version": "0.1", "message_id": "x", "kind": "health_check"})
}

fn internal_error() -> Value {
    json!({"schema_version": "0.1", "code": "internal", "message": "m"})
}

const ABOVE_WIRE_RANGE: u64 = 1 << 53;

/// Faults of identities and wire integers.
fn identity_faults() -> Vec<(&'static str, Value)> {
    let submit = golden_frame("stt_decode", 0);
    let completed = golden_frame("stt_decode", 2);
    let vad = golden_frame("vad_frames", 0);
    let release = golden_frame("session_release", 2);
    let above = json!(ABOVE_WIRE_RANGE);
    vec![
        ("null job_id", with(submit.clone(), "/job_id", Value::Null)),
        (
            "job_id above range",
            with(submit.clone(), "/job_id", above.clone()),
        ),
        (
            "session_key above range",
            with(submit.clone(), "/session_key", above.clone()),
        ),
        (
            "generation above range",
            with(submit.clone(), "/generation", above.clone()),
        ),
        (
            "start_sample above range",
            with(submit.clone(), "/input/start_sample", above.clone()),
        ),
        (
            "utterance_id above range",
            with(vad, "/input/utterance_id", above.clone()),
        ),
        (
            "word end above range",
            with(completed, "/result/words/0/end_sample", above),
        ),
        ("missing generation", without(submit, "/generation")),
        (
            "job_id on a session",
            with(release.clone(), "/job_id", json!(1)),
        ),
        (
            "unknown kind",
            with(release, "/kind", json!("session_reset")),
        ),
        (
            "session_key on health_check",
            with(health_check(), "/session_key", json!(1)),
        ),
        (
            "null job_id on a unary message",
            with(health_check(), "/job_id", Value::Null),
        ),
    ]
}

/// Faults of a `job_submit`.
fn submit_faults() -> Vec<(&'static str, Value)> {
    let submit = golden_frame("stt_decode", 0);
    let tts = golden_frame("tts_synthesis", 0);
    let vad = golden_frame("vad_frames", 0);
    let long_frame = with(vad.clone(), "/input/frame_samples", json!(16_385));
    let long_frame = with(long_frame, "/input/frame_count", json!(1));
    vec![
        ("unknown field", with(submit.clone(), "/job", json!(1))),
        ("missing input", without(submit.clone(), "/input")),
        ("missing job_class", without(submit.clone(), "/job_class")),
        (
            "missing progress_limit",
            without(submit.clone(), "/progress_limit"),
        ),
        ("missing options", without(submit.clone(), "/options")),
        (
            "correlation_id on a job",
            with(submit.clone(), "/correlation_id", json!("c")),
        ),
        (
            "tensors on a job",
            with(submit.clone(), "/tensors", json!([])),
        ),
        (
            "options as an array",
            with(submit.clone(), "/options", json!(["en"])),
        ),
        (
            "unknown option",
            with(tts.clone(), "/options/pitch", json!(1)),
        ),
        (
            "empty language",
            with(submit.clone(), "/options/language", json!("")),
        ),
        (
            "input as an array",
            with(tts.clone(), "/input", json!(["text_segment", 12])),
        ),
        (
            "format as an array",
            with(
                submit.clone(),
                "/input/format",
                json!(["pcm_s16le", 16000, 1]),
            ),
        ),
        (
            "unknown format field",
            with(submit.clone(), "/input/format/bits", json!(16)),
        ),
        (
            "unknown input field",
            with(submit.clone(), "/input/offset", json!(0)),
        ),
        (
            "frames on audio",
            with(submit, "/input/frame_count", json!(1)),
        ),
        (
            "start_sample on vad",
            with(vad, "/input/start_sample", json!(0)),
        ),
        (
            "frame longer than any window",
            with(long_frame, "/input/payload_length", json!(2)),
        ),
        (
            "empty voice",
            with(tts.clone(), "/options/voice", json!("")),
        ),
        (
            "zero speed",
            with(tts.clone(), "/options/speed_milli", json!(0)),
        ),
        (
            "voice without speed",
            without(tts.clone(), "/options/speed_milli"),
        ),
        (
            "speed above u16",
            with(tts, "/options/speed_milli", json!(65_536)),
        ),
    ]
}

/// Faults of the sidecar's job and session messages.
fn event_faults() -> Vec<(&'static str, Value)> {
    let accepted = golden_frame("stt_decode", 1);
    let completed = golden_frame("stt_decode", 2);
    let audio = golden_frame("tts_synthesis", 2);
    let vad = golden_frame("vad_frames", 2);
    let released = golden_frame("session_release", 6);
    vec![
        (
            "status on an event",
            with(accepted.clone(), "/status", json!("ok")),
        ),
        (
            "error on an event",
            with(accepted.clone(), "/error", internal_error()),
        ),
        (
            "error on session_released",
            with(released, "/error", internal_error()),
        ),
        (
            "result on accepted",
            with(accepted, "/result", completed["result"].clone()),
        ),
        (
            "progress_sequence on completed",
            with(completed.clone(), "/progress_sequence", json!(1)),
        ),
        (
            "completed without result",
            without(completed.clone(), "/result"),
        ),
        (
            "result as an array",
            with(vad, "/result", json!(["vad", [0.5]])),
        ),
        (
            "audio field on a transcript",
            with(completed.clone(), "/result/payload_length", json!(2)),
        ),
        (
            "unknown word field",
            with(completed, "/result/words/0/confidence", json!(1)),
        ),
        (
            "unknown result format field",
            with(audio, "/result/format/bits", json!(16)),
        ),
    ]
}

/// Faults of the error a `job_failed` carries.
fn error_faults() -> Vec<(&'static str, Value)> {
    let failed = golden_frame("cancel", 4);
    vec![
        ("failed without error", without(failed.clone(), "/error")),
        (
            "error as an array",
            with(failed.clone(), "/error", json!(["cancelled", "m"])),
        ),
        (
            "unknown error field",
            with(failed.clone(), "/error/detail", json!("x")),
        ),
        (
            "null error context",
            with(failed.clone(), "/error/context", Value::Null),
        ),
        (
            "error of another version",
            with(failed, "/error/schema_version", json!("9.9")),
        ),
    ]
}

/// Faults of the negotiation.
fn negotiation_faults() -> Vec<(&'static str, Value)> {
    let ready = golden_frame("negotiation", 0);
    let loaded = golden_frame("negotiation", 2);
    let health =
        json!({"ready": true, "backend_factory": null, "uptime_ns": 1, "last_error": null});
    let health_response = with(loaded.clone(), "/kind", json!("health_check_response"));
    let error_response = with(loaded.clone(), "/status", json!("error"));
    let seventeen: Vec<String> = (b'a'..=b'q').map(|c| char::from(c).to_string()).collect();
    vec![
        (
            "capabilities on prime",
            with(ready.clone(), "/kind", json!("prime")),
        ),
        (
            "null capabilities",
            with(ready.clone(), "/capabilities", Value::Null),
        ),
        (
            "empty capabilities",
            with(ready.clone(), "/capabilities", json!([])),
        ),
        (
            "seventeen capabilities",
            with(ready.clone(), "/capabilities", json!(seventeen)),
        ),
        (
            "capability name of 65 bytes",
            with(ready.clone(), "/capabilities", json!(["a".repeat(65)])),
        ),
        (
            "capability with a capital",
            with(ready.clone(), "/capabilities", json!(["Speech_jobs"])),
        ),
        (
            "duplicate capability",
            with(ready, "/capabilities", json!(["a", "a"])),
        ),
        (
            "job_classes on an error response",
            with(error_response, "/error", internal_error()),
        ),
        (
            "job_classes on a health response",
            with(health_response, "/health", health),
        ),
        (
            "empty job_classes",
            with(loaded.clone(), "/job_classes", json!([])),
        ),
        (
            "duplicate job class",
            with(loaded, "/job_classes", json!(["vad_frames", "vad_frames"])),
        ),
    ]
}

/// Frames one fault away from a golden frame. Both the schema and the mirror
/// refuse each of them.
fn single_faults() -> Vec<(&'static str, Value)> {
    let mut faults = identity_faults();
    faults.extend(submit_faults());
    faults.extend(event_faults());
    faults.extend(error_faults());
    faults.extend(negotiation_faults());
    faults
}

#[test]
fn single_fault_frames_fail_the_schema_and_the_mirror() {
    for (name, frame) in single_faults() {
        assert!(!schema_accepts(&frame), "{name}: schema accepts {frame}");
        assert!(mirror(&frame).is_err(), "{name}: mirror accepts {frame}");
    }
}

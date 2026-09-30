// SPDX-License-Identifier: Apache-2.0
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::Duration;

use serde_json::Value;
use tensorplate_cli::args::ProvisionArgs;
use tensorplate_cli::commands::bundle::{provision, ProvisionError};
use tensorplate_protocol::bundle::parse_bundle;

fn repo(path: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(path)
}

struct Origin {
    url: String,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<(String, usize)>>>,
    handle: Option<thread::JoinHandle<()>>,
}
impl Origin {
    fn new(mode: &'static str) -> Self {
        Self::new_at(mode, None)
    }
    fn new_at(mode: &'static str, occupied: Option<PathBuf>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let observed = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let handle = thread::spawn(move || {
            let root = repo("test/models/bundles/v0_1/smolvla_python_pytorch");
            while !stopping.load(Ordering::Relaxed) {
                let (mut socket, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(err) => panic!("accept: {err}"),
                };
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(socket.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let path = line
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .trim_start_matches('/')
                    .to_owned();
                let mut offset = 0;
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    if let Some(range) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                        offset = range.split('-').next().unwrap().parse().unwrap();
                    }
                }
                let index = observed.lock().unwrap().len();
                observed.lock().unwrap().push((path.clone(), offset));
                if index == 0 {
                    if let Some(destination) = &occupied {
                        fs::create_dir(destination).unwrap();
                        fs::write(destination.join("existing"), b"leave alone").unwrap();
                    }
                }
                let mut body = fs::read(root.join(&path)).unwrap();
                if mode == "corrupt" {
                    body[0] ^= 0x20;
                }
                if mode == "oversize" {
                    body.push(b'x');
                }
                if mode == "404" {
                    write!(
                        socket,
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                    continue;
                }
                if offset > 0 && mode != "no-range" {
                    write!(socket, "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {}-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", offset, body.len()-1, body.len(), body.len()-offset).unwrap();
                } else {
                    offset = 0;
                    write!(
                        socket,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .unwrap();
                }
                let interrupted = (index == 0 && matches!(mode, "interrupt" | "no-range"))
                    || (index == 1 && mode == "interrupt-second");
                let end = if interrupted {
                    body.len() / 2
                } else {
                    body.len()
                };
                let _ = socket.write_all(&body[offset..end]);
            }
        });
        Self {
            url,
            stop,
            requests,
            handle: Some(handle),
        }
    }
}
impl Drop for Origin {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.take().unwrap().join().unwrap();
    }
}

fn manifest(dir: &Path, url: &str) -> (PathBuf, Value) {
    let mut doc: Value = serde_json::from_str(
        &fs::read_to_string(repo(
            "protocol/rust/tests/fixtures/provisioning_manifest.json",
        ))
        .unwrap(),
    )
    .unwrap();
    for file in doc["bundles"][0]["files"].as_array_mut().unwrap() {
        file["url"] = Value::String(format!("{url}{}", file["path"].as_str().unwrap()));
    }
    let path = dir.join("m.json");
    fs::write(&path, doc.to_string()).unwrap();
    (path, doc)
}
fn args(manifest: &Path, into: &Path) -> ProvisionArgs {
    ProvisionArgs {
        name: "smolvla-fixture".into(),
        from: PathBuf::new(),
        manifest: Some(manifest.into()),
        into: Some(into.into()),
    }
}
fn snapshot(root: &Path) -> BTreeMap<PathBuf, (u64, i64, i64, Vec<u8>)> {
    let mut result = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for e in fs::read_dir(dir).unwrap() {
            let path = e.unwrap().path();
            let meta = fs::metadata(&path).unwrap();
            if meta.is_dir() {
                pending.push(path);
            } else {
                let bytes = fs::read(&path).unwrap();
                result.insert(path, (meta.ino(), meta.mtime(), meta.mtime_nsec(), bytes));
            }
        }
    }
    result
}

#[test]
fn interrupted_transfer_resumes_by_range_then_repeat_verifies_without_any_writes() {
    let origin = Origin::new("interrupt");
    let into = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let (manifest, doc) = manifest(config.path(), &origin.url);
    let args = args(&manifest, into.path());
    assert!(matches!(
        provision(&args),
        Err(ProvisionError::Fetch { .. })
    ));
    assert!(!into.path().join(&args.name).exists());
    assert!(!into.path().join(".smolvla-fixture.partial").exists());
    let first = &doc["bundles"][0]["files"][0];
    let digest = first["sha256"].as_str().unwrap();
    let expected = first["size"].as_u64().unwrap() / 2;
    let cache = into.path().join(".smolvla-fixture.download");
    assert_eq!(
        fs::metadata(cache.join(format!("{digest}.part")))
            .unwrap()
            .len(),
        expected
    );
    assert!(!cache.join("manifest.json").exists());
    assert!(parse_bundle(&cache).is_err());
    let report = provision(&args).expect("resumed");
    assert!(!report.already_provisioned);
    parse_bundle(&report.path).expect("deploy accepts unchanged");
    let requests = origin.requests.lock().unwrap().clone();
    assert_eq!(
        requests[1],
        (first["path"].as_str().unwrap().into(), expected as usize)
    );
    assert!(!cache.exists());
    let before = snapshot(into.path());
    let count = requests.len();
    let report = provision(&args).expect("second success");
    assert!(report.already_provisioned);
    assert_eq!(origin.requests.lock().unwrap().len(), count);
    assert_eq!(snapshot(into.path()), before);
}

#[test]
fn failed_fetches_never_publish_and_integrity_failures_remove_cached_bytes() {
    for (mode, token, kept) in [
        ("404", "fetch_failed", true),
        ("corrupt", "digest_mismatch", false),
        ("oversize", "size_mismatch", false),
    ] {
        let origin = Origin::new(mode);
        let into = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let (manifest, _) = manifest(config.path(), &origin.url);
        let err = provision(&args(&manifest, into.path())).unwrap_err();
        assert_eq!(err.code(), token, "{mode}: {err}");
        assert!(!into.path().join("smolvla-fixture").exists());
        assert!(!into.path().join(".smolvla-fixture.partial").exists());
        assert_eq!(
            into.path().join(".smolvla-fixture.download").exists(),
            kept,
            "{mode}"
        );
    }
}

#[test]
fn completed_cache_files_are_verified_and_skipped_on_retry() {
    for tampered in [false, true] {
        let origin = Origin::new("interrupt-second");
        let into = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let (manifest, doc) = manifest(config.path(), &origin.url);
        let args = args(&manifest, into.path());
        assert_eq!(provision(&args).unwrap_err().code(), "fetch_failed");
        let first = &doc["bundles"][0]["files"][0];
        let cache = into.path().join(".smolvla-fixture.download");
        let complete = cache.join(first["sha256"].as_str().unwrap());
        assert_eq!(fs::metadata(&complete).unwrap().len(), first["size"]);
        if tampered {
            let mut bytes = fs::read(&complete).unwrap();
            bytes[0] ^= 0x20;
            fs::write(&complete, bytes).unwrap();
            assert_eq!(provision(&args).unwrap_err().code(), "digest_mismatch");
            assert_eq!(origin.requests.lock().unwrap().len(), 2);
            assert!(!into.path().join(&args.name).exists());
        } else {
            provision(&args).unwrap();
            assert_eq!(
                origin
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(path, _)| path == first["path"].as_str().unwrap())
                    .count(),
                1
            );
        }
        assert!(!cache.exists());
        assert!(!into.path().join(".smolvla-fixture.partial").exists());
    }
}

#[test]
fn failed_publication_keeps_verified_cache_and_leaves_concurrent_destination_alone() {
    let into = tempfile::tempdir().unwrap();
    let destination = into.path().join("smolvla-fixture");
    let origin = Origin::new_at("normal", Some(destination.clone()));
    let config = tempfile::tempdir().unwrap();
    let (manifest, doc) = manifest(config.path(), &origin.url);
    let args = args(&manifest, into.path());
    assert_eq!(provision(&args).unwrap_err().code(), "io");
    assert_eq!(
        fs::read(destination.join("existing")).unwrap(),
        b"leave alone"
    );
    assert!(!destination.join("manifest.json").exists());
    assert!(!into.path().join(".smolvla-fixture.partial").exists());
    let cache = into.path().join(".smolvla-fixture.download");
    for file in doc["bundles"][0]["files"].as_array().unwrap() {
        assert_eq!(
            fs::metadata(cache.join(file["sha256"].as_str().unwrap()))
                .unwrap()
                .len(),
            file["size"]
        );
    }
    let requests = origin.requests.lock().unwrap().len();
    fs::remove_dir_all(&destination).unwrap();
    let report = provision(&args).unwrap();
    parse_bundle(&report.path).unwrap();
    assert_eq!(origin.requests.lock().unwrap().len(), requests);
    assert!(!cache.exists());
}

#[test]
fn origin_without_range_support_does_not_append_a_whole_file_to_partial_bytes() {
    let origin = Origin::new("no-range");
    let into = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let (manifest, _) = manifest(config.path(), &origin.url);
    let args = args(&manifest, into.path());
    assert_eq!(provision(&args).unwrap_err().code(), "fetch_failed");
    let before = snapshot(into.path());
    assert_eq!(provision(&args).unwrap_err().code(), "fetch_failed");
    assert_eq!(snapshot(into.path()), before);
}

#[test]
fn cache_links_unlisted_files_and_concurrent_runs_are_refused() {
    for mode in ["root-link", "file-link", "unsafe-mode", "unlisted", "busy"] {
        let origin = Origin::new("normal");
        let into = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let (manifest, doc) = manifest(config.path(), &origin.url);
        let args = args(&manifest, into.path());
        let cache = into.path().join(".smolvla-fixture.download");
        if mode == "root-link" {
            std::os::unix::fs::symlink(other.path(), &cache).unwrap();
        } else if mode == "busy" {
            fs::create_dir(into.path().join(".smolvla-fixture.partial")).unwrap();
        } else {
            fs::create_dir(&cache).unwrap();
            fs::set_permissions(&cache, fs::Permissions::from_mode(0o700)).unwrap();
            if mode == "unsafe-mode" {
                fs::set_permissions(&cache, fs::Permissions::from_mode(0o755)).unwrap();
            } else if mode == "unlisted" {
                fs::write(cache.join("other"), "unlisted").unwrap();
            } else {
                let digest = doc["bundles"][0]["files"][0]["sha256"].as_str().unwrap();
                std::os::unix::fs::symlink("/dev/null", cache.join(format!("{digest}.part")))
                    .unwrap();
            }
        }
        let token = provision(&args).unwrap_err().code();
        assert_eq!(
            token,
            match mode {
                "file-link" => "symbolic_link",
                "busy" => "partial_root_exists",
                _ => "fetch_failed",
            },
            "{mode}"
        );
        assert!(!into.path().join("smolvla-fixture").exists());
        assert!(origin.requests.lock().unwrap().is_empty());
        assert!(other.path().read_dir().unwrap().next().is_none());
    }
}

#[test]
fn packaged_sources_use_the_same_verification_and_refuse_links() {
    let into = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let (manifest, mut doc) = manifest(config.path(), "https://models.example.invalid/");
    for f in doc["bundles"][0]["files"].as_array_mut().unwrap() {
        let path = f["path"].as_str().unwrap().to_owned();
        f.as_object_mut().unwrap().remove("url");
        f["source_path"] = Value::String(path.clone());
        let dest = config.path().join(&path);
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::copy(
            repo("test/models/bundles/v0_1/smolvla_python_pytorch").join(path),
            dest,
        )
        .unwrap();
    }
    fs::write(&manifest, doc.to_string()).unwrap();
    let report = provision(&args(&manifest, into.path())).unwrap();
    parse_bundle(&report.path).unwrap();
    let into = tempfile::tempdir().unwrap();
    let f = &doc["bundles"][0]["files"][0];
    let path = config.path().join(f["path"].as_str().unwrap());
    let elsewhere = config.path().join("moved");
    fs::rename(&path, &elsewhere).unwrap();
    std::os::unix::fs::symlink(elsewhere, &path).unwrap();
    assert_eq!(
        provision(&args(&manifest, into.path())).unwrap_err().code(),
        "symbolic_link"
    );
    assert!(into.path().read_dir().unwrap().next().is_none());
}

#[test]
fn shipped_references_pin_exact_entry_sets_and_preserve_candidate_declarations() {
    use tensorplate_protocol::provisioning_manifest::ProvisioningManifest;
    let root = repo("packaging/provisioning");
    let manifest =
        ProvisioningManifest::parse(&fs::read_to_string(root.join("manifest.json")).unwrap())
            .unwrap();
    let schema: Value = serde_json::from_str(
        &fs::read_to_string(repo("protocol/schemas/bundle_manifest.json")).unwrap(),
    )
    .unwrap();
    let validator = jsonschema::JSONSchema::compile(&schema).unwrap();
    for (name, fixture, entry_name, count) in [
        (
            "stt-whisper-candidate",
            "stt_whisper_candidate",
            "stt-whisper-candidate.json",
            5,
        ),
        (
            "tts-kokoro-candidate",
            "tts_kokoro_candidate",
            "tts-kokoro-candidate.json",
            3,
        ),
    ] {
        let bundle = manifest.bundle(name).unwrap();
        assert_eq!(bundle.files.len(), count + 2);
        let mut reference: Value = serde_json::from_str(
            &fs::read_to_string(root.join(format!("bundles/{name}/{entry_name}"))).unwrap(),
        )
        .unwrap();
        let original: Value = serde_json::from_str(
            &fs::read_to_string(repo(&format!(
                "test/models/bundles/v0_1/{fixture}/{entry_name}"
            )))
            .unwrap(),
        )
        .unwrap();
        let assets = reference
            .as_object_mut()
            .unwrap()
            .remove("artifact_set")
            .unwrap();
        let mut declarations = original.clone();
        declarations.as_object_mut().unwrap().remove("artifact_set");
        assert_eq!(reference, declarations, "{name}: candidate declarations");
        let paths: Vec<_> = assets
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["path"].as_str().unwrap())
            .collect();
        let original_paths: Vec<_> = original["artifact_set"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["path"].as_str().unwrap())
            .collect();
        assert_eq!(paths, original_paths);
        let emitted: Value = serde_json::from_str(
            &fs::read_to_string(root.join(format!("bundles/{name}/manifest.json"))).unwrap(),
        )
        .unwrap();
        assert!(validator.is_valid(&emitted), "{name}: bundle schema");
        assert_eq!(
            emitted["runtime_compatibility"]["min_runtime_version"],
            "0.3.0"
        );
        assert_eq!(emitted["artifacts"].as_array().unwrap().len(), count + 1);
        check_reference_files(&root, name, bundle, &assets, &emitted);
    }
}

fn check_reference_files(
    root: &Path,
    name: &str,
    bundle: &tensorplate_protocol::provisioning_manifest::ProvisionedBundle,
    assets: &Value,
    emitted: &Value,
) {
    use sha2::{Digest, Sha256};
    for file in &bundle.files {
        if let Some(path) = &file.source_path {
            let bytes = fs::read(root.join(path)).unwrap();
            assert_eq!(bytes.len() as u64, file.size, "{}", file.path);
            assert_eq!(
                format!("{:x}", Sha256::digest(bytes)),
                file.sha256,
                "{}",
                file.path
            );
        } else {
            let url = file.url.as_deref().unwrap();
            let revision = if name.starts_with("stt") {
                "0a363e9161cbc7ed1431c9597a8ceaf0c4f78fcf"
            } else {
                "f3ff3571791e39611d31c381e3a41a3af07b4987"
            };
            assert!(url.contains(&format!("/resolve/{revision}/")));
            let asset = assets
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["path"] == file.path)
                .expect("every remote file is entry-listed");
            assert_eq!(asset["digest"], format!("sha256:{}", file.sha256));
        }
        if file.path != "manifest.json" {
            let artifact = emitted["artifacts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["path"] == file.path)
                .unwrap();
            assert_eq!(artifact["digest"], format!("sha256:{}", file.sha256));
            assert_eq!(artifact["byte_size"], file.size);
        }
    }
}

#[test]
fn fetch_failures_keep_the_documented_error_envelope() {
    use tensorplate_cli::{CliError, ExitCode};
    for (error, token) in [
        (
            ProvisionError::Fetch {
                path: "file".into(),
                reason: "curl exit 28".into(),
            },
            "fetch_failed",
        ),
        (
            ProvisionError::FetchSourceMissing {
                path: "file".into(),
            },
            "fetch_source_missing",
        ),
    ] {
        let error: CliError = error.into();
        assert_eq!(error.context(), Some(token));
        assert_eq!(error.exit_code(), ExitCode::ProvisionFailed);
        assert_eq!(error.protocol_code().as_str(), "load_failed");
    }
}

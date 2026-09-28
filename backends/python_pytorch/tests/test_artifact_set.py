"""Artifact-set resolution against the candidate speech bundles."""

from __future__ import annotations

import hashlib
import json
import os
import shutil
from pathlib import Path
from typing import Any

import pytest

from tensorplate_pytorch_backend import protocol
from tensorplate_pytorch_backend.artifact_set import ArtifactSet, load_artifact_set
from tensorplate_pytorch_backend.backends.base import BackendError

_BUNDLES = Path(__file__).resolve().parents[3] / "test" / "models" / "bundles" / "v0_1"
_STT = ("stt_whisper_candidate", "stt-whisper-candidate.json")
_TTS = ("tts_kokoro_candidate", "tts-kokoro-candidate.json")
_CANARY = "tp-canary-7f3a9c"


def _entry(root: Path, name: str) -> dict[str, Any]:
    loaded: dict[str, Any] = json.loads((root / name).read_text(encoding="utf-8"))
    return loaded


def _copy(tmp_path: Path, bundle: tuple[str, str]) -> tuple[Path, dict[str, Any]]:
    root = tmp_path / "bundle"
    shutil.copytree(_BUNDLES / bundle[0], root)
    return root / bundle[1], _entry(root, bundle[1])


def _listed(relative_path: str, body: bytes) -> dict[str, str]:
    return {"path": relative_path, "digest": f"sha256:{hashlib.sha256(body).hexdigest()}"}


def _refusal(entry_path: Path, entry: dict[str, Any]) -> BackendError:
    with pytest.raises(BackendError) as caught:
        load_artifact_set(entry_path, entry)
    assert _CANARY not in caught.value.code_message
    return caught.value


@pytest.mark.parametrize("bundle", [_STT, _TTS])
def test_candidate_entries_verify_every_listed_file(bundle: tuple[str, str]) -> None:
    root = _BUNDLES / bundle[0]
    entry = _entry(root, bundle[1])
    artifacts = load_artifact_set(root / bundle[1], entry)
    assert sorted(artifacts.files) == sorted(item["path"] for item in entry["artifact_set"])
    for relative_path, resolved in artifacts.files.items():
        assert artifacts.file(relative_path) == resolved == (root / relative_path).resolve()


def test_whisper_candidate_model_directory_is_handed_over_whole() -> None:
    root = _BUNDLES / _STT[0]
    artifacts = load_artifact_set(root / _STT[1], _entry(root, _STT[1]))
    assert artifacts.directory("model") == (root / "model").resolve()


def test_kokoro_candidate_voice_is_a_listed_file() -> None:
    root = _BUNDLES / _TTS[0]
    artifacts = load_artifact_set(root / _TTS[1], _entry(root, _TTS[1]))
    voice = artifacts.file("model/voices/af_heart.pt")
    assert voice.name == "af_heart.pt"
    assert artifacts.directory("model") == (root / "model").resolve()


def test_a_changed_file_fails_its_digest(tmp_path: Path) -> None:
    entry_path, entry = _copy(tmp_path, _TTS)
    (entry_path.parent / "model" / "kokoro-v1_0.pth").write_bytes(b"changed")
    err = _refusal(entry_path, entry)
    assert err.code == protocol.ERR_LOAD_FAILED
    assert err.code_message == "artifact_set item 1 does not match its digest"


def test_messages_name_the_item_by_index_not_by_path(tmp_path: Path) -> None:
    entry_path, entry = _copy(tmp_path, _TTS)
    voice = entry_path.parent / "model" / "voices" / f"{_CANARY}.pt"
    voice.write_bytes(b"voice")
    entry["artifact_set"].append(
        {"path": f"model/voices/{_CANARY}.pt", "digest": "sha256:" + "0" * 64}
    )
    err = _refusal(entry_path, entry)
    assert err.code_message == "artifact_set item 3 does not match its digest"


def test_a_missing_file_fails_load(tmp_path: Path) -> None:
    entry_path, entry = _copy(tmp_path, _STT)
    (entry_path.parent / "model" / "model.bin").unlink()
    err = _refusal(entry_path, entry)
    assert err.code == protocol.ERR_LOAD_FAILED
    assert err.code_message == "artifact_set item 1 is missing or unreadable"


@pytest.mark.parametrize(
    "path",
    [
        "",
        "/etc/passwd",
        "../outside",
        "model/../model/model.bin",
        "./model/model.bin",
        "model//model.bin",
        "model/",
        "model\\model.bin",
        "model/model.bin\0",
    ],
)
def test_unsafe_paths_are_refused_before_any_file_is_opened(tmp_path: Path, path: str) -> None:
    entry_path, entry = _copy(tmp_path, _STT)
    entry["artifact_set"][0]["path"] = path
    err = _refusal(entry_path, entry)
    assert err.code == protocol.ERR_CONFIG_INVALID
    assert err.code_message == "artifact_set item 0 has an unsafe path"


def test_a_symlink_that_leaves_the_entry_directory_is_refused(tmp_path: Path) -> None:
    entry_path, entry = _copy(tmp_path, _STT)
    outside = tmp_path / f"{_CANARY}.bin"
    outside.write_bytes(b"outside")
    link = entry_path.parent / "model" / "linked.bin"
    link.symlink_to(outside)
    entry["artifact_set"].append(_listed("model/linked.bin", b"outside"))
    err = _refusal(entry_path, entry)
    assert err.code == protocol.ERR_LOAD_FAILED
    assert err.code_message == "artifact_set item 5 resolves outside the entry's directory"


def test_a_symlink_that_stays_inside_the_entry_directory_is_verified(tmp_path: Path) -> None:
    entry_path, entry = _copy(tmp_path, _STT)
    (entry_path.parent / "model" / "linked.bin").symlink_to("model.bin")
    body = (entry_path.parent / "model" / "model.bin").read_bytes()
    entry["artifact_set"].append(_listed("model/linked.bin", body))
    artifacts = load_artifact_set(entry_path, entry)
    assert (
        artifacts.file("model/linked.bin") == (entry_path.parent / "model" / "model.bin").resolve()
    )


def test_a_listed_directory_is_not_a_regular_file(tmp_path: Path) -> None:
    entry_path, entry = _copy(tmp_path, _TTS)
    entry["artifact_set"].append(_listed("model/voices", b""))
    err = _refusal(entry_path, entry)
    assert err.code == protocol.ERR_LOAD_FAILED
    assert err.code_message == "artifact_set item 3 is not a regular file"


@pytest.mark.parametrize(
    ("mutate", "message"),
    [
        (lambda e: e.pop("artifact_set"), "the entry must list a non-empty artifact_set"),
        (lambda e: e.update(artifact_set=[]), "the entry must list a non-empty artifact_set"),
        (lambda e: e.update(artifact_set={}), "the entry must list a non-empty artifact_set"),
        (
            lambda e: e["artifact_set"][0].update(size=1),
            "artifact_set item 0 must hold exactly `path` and `digest`",
        ),
        (
            lambda e: e["artifact_set"].append(dict(e["artifact_set"][0])),
            "artifact_set item 5 repeats a path",
        ),
        (
            lambda e: e["artifact_set"][0].update(digest="md5:" + "0" * 32),
            "artifact_set item 0 needs a sha256:<64 hex> digest",
        ),
        (
            lambda e: e["artifact_set"][0].update(digest="sha256:" + "0" * 63),
            "artifact_set item 0 needs a sha256:<64 hex> digest",
        ),
        (
            lambda e: e["artifact_set"][0].update(digest=None),
            "artifact_set item 0 needs a sha256:<64 hex> digest",
        ),
    ],
)
def test_malformed_entries_are_configuration_errors(
    tmp_path: Path, mutate: Any, message: str
) -> None:
    entry_path, entry = _copy(tmp_path, _STT)
    mutate(entry)
    err = _refusal(entry_path, entry)
    assert err.code == protocol.ERR_CONFIG_INVALID
    assert err.code_message == message


def test_uppercase_hex_digests_match(tmp_path: Path) -> None:
    entry_path, entry = _copy(tmp_path, _STT)
    for item in entry["artifact_set"]:
        item["digest"] = "sha256:" + item["digest"].split(":", 1)[1].upper()
    assert len(load_artifact_set(entry_path, entry).files) == 5


def test_a_file_the_entry_does_not_list_is_not_handed_out() -> None:
    root = _BUNDLES / _TTS[0]
    artifacts = load_artifact_set(root / _TTS[1], _entry(root, _TTS[1]))
    for unlisted in ("manifest.json", _TTS[1], "model/voices/other.pt", "model/voices"):
        with pytest.raises(BackendError) as caught:
            artifacts.file(unlisted)
        assert caught.value.code == protocol.ERR_CONFIG_INVALID


def _loaded(tmp_path: Path, bundle: tuple[str, str]) -> ArtifactSet:
    entry_path, entry = _copy(tmp_path, bundle)
    return load_artifact_set(entry_path, entry)


def test_a_directory_holding_an_unlisted_file_is_refused(tmp_path: Path) -> None:
    artifacts = _loaded(tmp_path, _STT)
    (artifacts.root / "model" / f"{_CANARY}.json").write_text("{}", encoding="utf-8")
    with pytest.raises(BackendError) as caught:
        artifacts.directory("model")
    assert caught.value.code == protocol.ERR_LOAD_FAILED
    assert _CANARY not in caught.value.code_message


def test_a_directory_holding_an_unlisted_nested_file_is_refused(tmp_path: Path) -> None:
    artifacts = _loaded(tmp_path, _TTS)
    (artifacts.root / "model" / "voices" / "other.pt").write_bytes(b"voice")
    with pytest.raises(BackendError) as caught:
        artifacts.directory("model")
    assert caught.value.code == protocol.ERR_LOAD_FAILED


def test_a_directory_holding_a_linked_directory_is_refused(tmp_path: Path) -> None:
    artifacts = _loaded(tmp_path, _STT)
    empty = tmp_path / "elsewhere"
    empty.mkdir()
    (artifacts.root / "model" / "nested").symlink_to(empty, target_is_directory=True)
    with pytest.raises(BackendError) as caught:
        artifacts.directory("model")
    assert caught.value.code == protocol.ERR_LOAD_FAILED


def test_a_linked_directory_is_not_handed_over(tmp_path: Path) -> None:
    artifacts = _loaded(tmp_path, _STT)
    os.symlink(artifacts.root / "model", artifacts.root / "alias")
    with pytest.raises(BackendError) as caught:
        artifacts.directory("alias")
    assert caught.value.code == protocol.ERR_LOAD_FAILED
    assert caught.value.code_message == "an artifact_set directory is a symlink"


def test_a_directory_below_a_link_that_leaves_the_entry_directory_is_refused(
    tmp_path: Path,
) -> None:
    entry_path, entry = _copy(tmp_path, _STT)
    root = entry_path.parent
    outside = tmp_path / "outside"
    (outside / "sub").mkdir(parents=True)
    (outside / "sub" / "model.bin").symlink_to(root / "model" / "model.bin")
    (root / "alias").symlink_to(outside, target_is_directory=True)
    body = (root / "model" / "model.bin").read_bytes()
    entry["artifact_set"].append(_listed("alias/sub/model.bin", body))
    artifacts = load_artifact_set(entry_path, entry)
    with pytest.raises(BackendError) as caught:
        artifacts.directory("alias/sub")
    assert caught.value.code == protocol.ERR_LOAD_FAILED
    assert caught.value.code_message == (
        "an artifact_set directory resolves outside the entry's directory"
    )


@pytest.mark.skipif(os.geteuid() == 0, reason="root lists a directory it cannot read")
@pytest.mark.parametrize("unreadable", ["model", "model/voices"])
def test_a_directory_that_cannot_be_listed_is_refused(tmp_path: Path, unreadable: str) -> None:
    artifacts = _loaded(tmp_path, _TTS)
    (artifacts.root / unreadable / f"{_CANARY}.pt").write_bytes(b"unlisted")
    (artifacts.root / unreadable).chmod(0o311)
    try:
        with pytest.raises(BackendError) as caught:
            artifacts.directory("model")
    finally:
        (artifacts.root / unreadable).chmod(0o755)
    assert caught.value.code == protocol.ERR_LOAD_FAILED
    assert caught.value.code_message == "an artifact_set directory cannot be listed"


@pytest.mark.parametrize(
    ("relative_path", "code"),
    [
        ("../bundle/model", protocol.ERR_CONFIG_INVALID),
        ("", protocol.ERR_CONFIG_INVALID),
        ("absent", protocol.ERR_LOAD_FAILED),
        (".", protocol.ERR_CONFIG_INVALID),
    ],
)
def test_directory_paths_follow_the_same_rules(
    tmp_path: Path, relative_path: str, code: str
) -> None:
    artifacts = _loaded(tmp_path, _STT)
    with pytest.raises(BackendError) as caught:
        artifacts.directory(relative_path)
    assert caught.value.code == code


def test_a_directory_the_entry_lists_nothing_in_is_refused(tmp_path: Path) -> None:
    artifacts = _loaded(tmp_path, _STT)
    (artifacts.root / "empty").mkdir()
    with pytest.raises(BackendError) as caught:
        artifacts.directory("empty")
    assert caught.value.code == protocol.ERR_CONFIG_INVALID

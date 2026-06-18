"""High-level vision detection client built on the serving client."""

from __future__ import annotations

from collections.abc import Sequence
from pathlib import Path
from typing import TYPE_CHECKING

from tensorplate.client import DEFAULT_TIMEOUT_S
from tensorplate.conventions import YOLO_V8_SINGLE_OUTPUT, detections
from tensorplate.errors import ProtocolError
from tensorplate.postprocess import Detection, decode_detections
from tensorplate.preprocess import PreprocessConfig, preprocess
from tensorplate.serving import InferResult, ServingClient
from tensorplate.tensors import TensorOutput

if TYPE_CHECKING:
    import numpy as np

_DETECTION_TAGS = frozenset({detections.boxes, detections.scores, detections.classes})


class VisionClient:
    """High-level object-detection client over a deployed detector.

    Composes client-side preprocessing, ``ServingClient.infer``, and YOLO
    postprocessing into a single :meth:`detect` call. Synchronous in
    v0.1.3. Pass an existing :class:`ServingClient` via ``client``, or let
    the constructor resolve one with the same precedence as the CLI.
    """

    def __init__(
        self,
        serving_url: str | None = None,
        *,
        profile: str | None = None,
        config_path: str | None = None,
        timeout: float = DEFAULT_TIMEOUT_S,
        discover: bool = True,
        client: ServingClient | None = None,
    ) -> None:
        self._serving = client or ServingClient(
            serving_url,
            profile=profile,
            config_path=config_path,
            timeout=timeout,
            discover=discover,
        )

    @property
    def serving(self) -> ServingClient:
        """The underlying serving client."""
        return self._serving

    def detect(
        self,
        image: str | bytes | Path | np.ndarray,
        *,
        endpoint: str,
        input_name: str = "images",
        output_name: str | None = None,
        score_threshold: float = 0.25,
        nms_threshold: float = 0.45,
        labels: Sequence[str] | None = None,
        transposed: bool = False,
        contract: str = YOLO_V8_SINGLE_OUTPUT,
        preprocess_config: PreprocessConfig | None = None,
    ) -> list[Detection]:
        """Detect objects in ``image`` using the deployed model at ``endpoint``.

        Preprocesses the image, runs one synchronous inference, and decodes
        the detector output into source-pixel :class:`Detection` results.
        Requires the ``tensorplate-python[vision]`` extra. The detection
        output tensor is chosen explicitly (``output_name``), or — for a
        single-output response — automatically, or by a ``detections.*``
        ``semantic_tag``; an ambiguous response fails clearly.
        """
        if preprocess_config is not None:
            config = preprocess_config
        else:
            config = PreprocessConfig(input_name=input_name)
        tensor, transform = preprocess(image, config)
        result = self._serving.infer(endpoint, [tensor])
        output = _select_detection_output(result, output_name)
        return decode_detections(
            output,
            transform,
            score_threshold=score_threshold,
            nms_threshold=nms_threshold,
            labels=labels,
            transposed=transposed,
            contract=contract,
        )


def _select_detection_output(result: InferResult, output_name: str | None) -> TensorOutput:
    if output_name is not None:
        for output in result.outputs:
            if output.name == output_name:
                return output
        available = [output.name for output in result.outputs]
        raise ProtocolError(
            f"serving response has no output named {output_name!r}; available: {available}"
        )
    if len(result.outputs) == 1:
        return result.outputs[0]
    tagged = [output for output in result.outputs if output.semantic_tag in _DETECTION_TAGS]
    if len(tagged) == 1:
        return tagged[0]
    available = [output.name for output in result.outputs]
    raise ProtocolError(
        f"serving response has {len(result.outputs)} outputs; pass output_name to choose the "
        f"detection tensor (available: {available}), or tag exactly one output with a "
        "detections.* semantic_tag"
    )

# Test model fixtures

Small model artifacts used by tests.

Rules:

- Committed fixtures must be small (typically &lt;= 1 MB).
- Each fixture documents its provenance, license, and how it was produced.
- Larger fixtures (full TensorRT engines, real ONNX networks) are fetched
  on demand by setup scripts and not committed to the repository.

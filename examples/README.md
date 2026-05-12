# `examples/`

Small, runnable programs that demonstrate the public C++ runtime API.
These are NOT part of the v0.1.0 product surface — they exist so a
developer can quickly verify a clean build by exercising the runtime
end to end without spinning up the full serving worker.

## Targets

- `tp_example_buffer_plane` (`tensorplate-example-buffer-plane`) —
  walks the V01-E03 buffer plane: ingress copy, `BufferRef` +
  `TensorView` lifetime, `InferRequest` build, cancellation cleanup,
  partial-output cleanup, mock policy execution, `InferResult`
  construction, and memory-pressure events.

## Building

```bash
cmake -S . -B build -DCMAKE_BUILD_TYPE=Debug
cmake --build build --target tp_example_buffer_plane
./build/examples/tensorplate-example-buffer-plane
```

Expected output ends with `OK`. Non-zero exit codes indicate a
buffer-plane invariant violation; investigate before shipping.

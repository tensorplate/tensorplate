# CLI Exit Codes

The `tensorplate` binary maps every typed `CliError` to a stable exit code. The
V01-E15 validation harness and shell scripts may assert on these.

| Code | Name              | When                                                                  |
| ---: | :---------------- | :-------------------------------------------------------------------- |
|    0 | success           | Command succeeded.                                                    |
|    1 | failure           | Generic failure with no specific bucket (IO, serialization, internal).|
|    2 | usage             | Argv parse error, config file rejected, or local bundle path invalid. |
|    3 | agent_error       | Agent reachable but rejected the request with a typed error.          |
|    4 | transport         | Agent unreachable or transport timed out.                             |
|    5 | busy              | A concurrent agent transaction is in flight.                          |
|    6 | unavailable       | Operation is structurally unavailable (e.g. rollback with no previous active, reserved profile mode). |
|   10 | doctor_findings   | `doctor` returned at least one `fail` finding.                        |
|   11 | inference_failed  | `infer` got a typed failure from the serving worker.                  |

New CLI errors must extend this table rather than re-use a code that already
means something different.

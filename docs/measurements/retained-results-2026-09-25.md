# Retained-result access measurement

A literal search located a synthetic failure near the end of a 10 MiB log with
785–786 bytes of JSON per reader. The original log remained available through
an authenticated download, whose complete bytes and SHA-256 digest matched the
fixture. This supports using `result.select` for model evidence and downloads
for complete files.

Both access modes used the same debug binary, Rust source, Python harness,
fixture, and eight concurrent readers. Each mode started a fresh server and
loopback upstream process. The raw records contain the binary, source and
harness SHA-256 identities:

- [Selective access](retained-results-2026-09-25-selection.json)
- [Whole-resource compatibility access](retained-results-2026-09-25-compatibility.json)

| Measurement | Literal search | Whole-resource read |
| --- | ---: | ---: |
| Original payload | 10,485,760 bytes | 10,485,760 bytes |
| Initial retained-result reply | 5,589 bytes | 5,589 bytes |
| JSON response per reader | 785–786 bytes | 10,962,520–10,962,521 bytes |
| Median access latency | 0.125 seconds | 6.278 seconds |
| Maximum access latency | 0.137 seconds | 7.369 seconds |
| Server peak resident memory after access | 109,280 KiB | 211,412 KiB |
| Server resident memory before access | 66,996 KiB | 66,236 KiB |
| Server resident memory after access | 67,428 KiB | 81,908 KiB |

JSON byte counts include the RPC envelope but exclude SSE framing. They are
not model token counts. Latency includes local HTTP, serialization, and Python
client decoding. Memory comes from Linux `/proc` for the server process; it
excludes client memory and is not an allocation count. Peak memory includes
startup and the initial upstream result. The original upstream read still
buffers the bounded object before retaining it.

This is one local run of each mode on a debug build, not a production throughput
or tail-latency estimate. Whole-resource compatibility deliberately returns the
complete payload, so the modes fulfill different retrieval needs. The comparison
demonstrates the cost avoided when a caller needs only bounded evidence.
Independent regression tests cover Unicode continuation, JSON projection,
session ownership, grant expiry, download capacity, and retaining the shared
memory charge until the last active reader drops it.

Reproduce from the repository root after building the candidate:

```sh
cargo build -p gitea-server --locked
python3 scripts/measure_results.py target/debug/mcp-gitea-rs --mode selection
python3 scripts/measure_results.py target/debug/mcp-gitea-rs --mode compatibility
```

The harness uses synthetic credentials and disposable loopback infrastructure.
It records aggregate measurements only and terminates its server afterward.

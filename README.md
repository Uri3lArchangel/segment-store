# Segment Store

Crash recovery for a dependency-free Rust key/value store. Every acknowledged mutation calls fdatasync-equivalent `sync_data`. On open, the engine replays checksummed frames to rebuild a key-to-offset index, truncates an incomplete terminal frame, and refuses complete frames that fail validation.

## Run

Requires Rust 1.89+ and a Linux local filesystem:

```sh
cargo test
cargo run -- /tmp/demo-segment.seg put status ready
cargo run -- /tmp/demo-segment.seg get status
cargo run -- /tmp/demo-segment.seg verify
cargo run -- /tmp/demo-segment.seg delete status
```

`get` prints the raw value without a newline and exits 2 for a missing key. `verify` opens, validates, and **repairs** a partial tail; inspect a copy if you need forensic preservation. An exclusive advisory lock prevents cooperating processes from opening the same file concurrently. Do not edit, replace or unlink the file independently while a writer is running.

## Demonstrate tail recovery

Use an expendable file, then emulate a crash that preserves only a prefix of the last write:

```sh
cargo build --release
bin=./target/release/segment-store
dir=$(mktemp -d)
"$bin" "$dir/test.seg" put status ready
"$bin" "$dir/test.seg" put status writing
truncate -s -5 "$dir/test.seg"
"$bin" "$dir/test.seg" verify
test "$("$bin" "$dir/test.seg" get status)" = ready
rm -r "$dir"
```

The verified demo kept `ready` and removed 24 incomplete bytes. The integration suite also covers 25 cuts through the final frame and one-bit mutations at all 53 byte positions in a two-record fixture.

## File format

| Field | Bytes |
| --- | ---: |
| File header | 8, ASCII SEGLDG01 |
| Key length | 4, little-endian |
| Value length | 4, little-endian; 0xffffffff means tombstone |
| CRC-32 of lengths | 4, little-endian |
| Key and value | Variable; tombstones have no value |
| CRC-32 of lengths + key + value | 4, little-endian |

The length checksum is separate so a corrupted complete length field cannot be mistaken for an incomplete payload. Replay rejects unknown format, invalid bounded lengths and bad CRCs before interpreting operations. A checksum is not a cryptographic authentication mechanism. The live index holds keys and offsets, not values. Replay is linear in log size plus index operations.

## Durability and boundaries

- A put/delete becomes visible after its frame has been written and synced. An I/O error poisons the open handle; reopen to recover. An operation interrupted before acknowledgement can still be replayed if its full frame persisted. Retrying is an application-level choice.
- Directory metadata is synced on open; supported model is a single existing parent directory and local disk honoring sync and locks. There is no replication, multiple-writer concurrency, compaction, encryption, hot bit-rot verification or cross-key transaction support.
- Tail recovery models a file truncated at a byte boundary. It is not a guarantee about arbitrary torn sectors, retained zero blocks or faulty firmware. If a file was independently truncated at a record boundary, no metadata tells the engine which acknowledged records were lost.
- The tests model deterministic replay against a reference map, all byte-boundary cuts in a final write, single-bit corruption in complete records, bounded record sizes, and a separately spawned CLI competing with an existing lock. These tests do not simulate physical power loss.

## Benchmark

```sh
cargo run --release --example microbench -- /tmp/new-benchmark.seg 10000
```

Use a fresh path on the actual storage device. The benchmark performs serial 256-byte durable writes, reports write latency percentiles, then rebuilds the index and checks the replay on reopening. Record the OS, filesystem, device type and multiple runs. Warm cache and virtualized storage can dominate; this is not a database comparison or performance guarantee.

Reference measurement: Rust 1.89.0, Linux 6.18.57, an ext4 cloud volume, AMD EPYC 9V74 processor exposed by the VM. Each run used a new file, 10,000 sequential writes, 8-byte keys and 256-byte values (2,800,008 bytes on disk).

| Run | Writes/s | Write p50 | Write p95 | Reopen and replay |
| --- | ---: | ---: | ---: | ---: |
| 1 | 1,199 | 709 µs | 1,499 µs | 15.55 ms |
| 2 | 1,807 | 473 µs | 954 µs | 19.62 ms |
| 3 | 2,115 | 420 µs | 771 µs | 18.50 ms |

These runs demonstrate storage variability rather than a throughput target. Virtual storage below ext4 was not independently characterized.

Portfolio prototype. The tests and benchmarks describe local behavior, not a production deployment.

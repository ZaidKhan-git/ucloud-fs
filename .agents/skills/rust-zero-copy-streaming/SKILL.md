---
name: rust-zero-copy-streaming
description: Buffer and stream management in Rust for high-throughput I/O pipelines, using bytes::Bytes and BytesMut, tokio::io, reqwest or hyper body streams, chunk caches, and filesystem read/write callbacks. Use this skill whenever the user mentions zero-copy, Bytes, BytesMut, buffer copies, allocations, memory bloat, streaming downloads or uploads, chunk caching, backpressure, read-ahead, hashing a stream, or says the daemon burns CPU or RAM moving data. Use it even if they only ask "how do I avoid copying this response body" or "why is memory growing while streaming".
---

# Rust Zero-Copy Streaming

## The idea, from the ground up

A copy costs time (the CPU moves every byte) and memory (a second buffer exists). Data passing through a pipeline (network, cache, hash, operating system) can be copied at every hop or shared. Rust's `bytes::Bytes` makes sharing cheap: it is a reference-counted view of a buffer. Cloning it or taking a sub-slice copies **no data**, only bumps a counter and adjusts two numbers (start and length).

So the skill is: **keep data in `Bytes` for as long as possible, and be deliberate about the few places where a copy is truly required.**

## Honest limit: zero-copy ends at the OS boundary

It is tempting to believe an HTTP chunk can be handed straight to the OS read buffer by moving pointers. In practice:

- **Read replies.** `fuser`'s `reply.data(&[u8])` copies your bytes into the kernel message. A WinFsp read callback gives you a buffer that *you must fill*. Either way, **one copy into the OS buffer is unavoidable** (unless you use special kernel features like FUSE passthrough or `splice`, which are far outside normal use).
- **Write callbacks.** The OS gives you a borrowed `&[u8]` that is only valid until the callback returns. To hand it to an async task you must copy it once into an owned buffer (`Bytes::copy_from_slice`).

So the realistic target is **one copy at each OS edge and none in between**, not zero overall. Do not promise more.

## Core tools

| Type | Use |
|---|---|
| `Bytes` | Immutable, cheaply cloneable, O(1) `slice(range)`; the currency of the pipeline |
| `BytesMut` | Growable buffer you fill, then `freeze()` into `Bytes` without copying |
| `Buf` / `BufMut` | Traits for reading and writing across buffers; `Buf::chain` joins buffers without merging them |
| `tokio_util::io::StreamReader` | Turns a stream of `Bytes` into an `AsyncRead` |
| `tokio_util::io::ReaderStream` | Turns an `AsyncRead` into a stream of `Bytes` |

## Patterns

### Consume a response body without joining it

```rust
use futures::StreamExt;
use sha2::{Digest, Sha256};

let mut stream = response.bytes_stream();      // yields Bytes frames
let mut hasher = Sha256::new();
while let Some(frame) = stream.next().await {
    let frame: bytes::Bytes = frame?;
    hasher.update(&frame);                      // hash in place, no copy
    cache.push(frame);                          // move the Bytes, no copy
}
```

Avoid `Vec::extend_from_slice` per frame in the hot path, since it copies every byte and may reallocate repeatedly. Keep a `Vec<Bytes>` (or a `Buf::chain`) and only flatten if something truly needs contiguous memory.

### Serve a read from a chunk cache

Key the cache by `(file_id, chunk_index)` and store `Bytes`. A `read(offset, len)` becomes: find the chunk, `chunk.slice(a..b)`, then perform the single copy into the OS reply. If the request spans two chunks, copy from each slice in turn. No intermediate buffer.

### Fill a buffer then freeze

```rust
let mut buf = bytes::BytesMut::with_capacity(chunk_size);
while buf.len() < chunk_size {
    if reader.read_buf(&mut buf).await? == 0 { break; }
}
let chunk: bytes::Bytes = buf.freeze();         // no copy
```

## The traps

- **A small slice keeps the whole allocation alive.** `big.slice(10..20)` shares the original multi-megabyte buffer, which stays in memory until every view is dropped. If you retain a tiny piece for a long time (a metadata field, a cached name), copy it out with `Bytes::copy_from_slice` so the large buffer can be freed.
- **Unbounded buffering.** Read-ahead and download tasks that outrun the consumer fill memory. Use bounded channels and a memory budget, for example a `tokio::sync::Semaphore` where each in-flight chunk holds permits proportional to its size. Peak memory is roughly *in-flight chunks × chunk size*, so choose both deliberately.
- **Copying to satisfy the borrow checker.** If you find yourself calling `.to_vec()` to escape a lifetime error, stop and ask whether an owned `Bytes` should have been passed in the first place.
- **Mutating shared data.** `Bytes` is immutable. If you need to modify (for example encrypt in place), start from `BytesMut`, or accept one copy.
- **Hidden copies in conversions.** `Vec<u8>` into `Bytes` is free (`Bytes::from(vec)` takes ownership), but `&[u8]` into `Bytes` copies. `String::into_bytes` is free, `as_bytes().to_vec()` is not.

## Measure before optimising

Do not trust intuition about where copies happen.

1. Use `criterion` for throughput benchmarks of the read path with realistic chunk sizes.
2. Use `dhat` (heap profiler) or a counting allocator to count allocations per request. A well-designed read path should allocate close to nothing per call once the cache is warm.
3. Use `cargo flamegraph` to see whether `memcpy` actually dominates. If network latency dominates (it usually does for a remote-backed filesystem), buffer tuning will not move the needle and prefetching will.
4. Track resident memory under a sustained streaming load to catch retention bugs.

## Where to spend effort first (in order of usual payoff)

1. Read-ahead and caching, which hide network latency.
2. Bounding memory with backpressure, which prevents crashes.
3. Removing accidental copies inside the pipeline.
4. Micro-optimising the final OS copy, which is rarely worth it.

## Review checklist

1. Does data stay in `Bytes` between network and OS edge, with at most one copy at each edge?
2. Is every long-lived small slice copied out of its large parent?
3. Are channels and in-flight buffers bounded, with a stated memory budget?
4. Is hashing done per frame rather than after joining?
5. Are `.to_vec()` and `extend_from_slice` absent from the hot path, or justified in a comment?
6. Is there a benchmark and an allocation count backing each optimisation claim?

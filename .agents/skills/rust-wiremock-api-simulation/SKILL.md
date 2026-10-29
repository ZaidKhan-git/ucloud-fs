---
name: "rust-wiremock-api-simulation"
description: Offline, deterministic simulation of remote REST APIs in Rust tests using wiremock, with injected rate limits, expired tokens, flaky responses and resumable-upload protocols (modeled on Google Drive). Use this skill whenever the user mentions wiremock, mock server, HTTP 429, backoff, retry logic, OAuth token refresh testing, invalid_grant, resumable uploads, Content-Range, 308 responses, chaos testing an API client, or is tired of hitting real Google rate limits while debugging. Use it even if they only say "how do I test my retry code" or "my tests call the real API".
---

# Rust API Simulation with wiremock

## Why simulate

Debugging a client against the real API is slow, rate-limited and unrepeatable. You cannot make Google return a 429 on demand, expire a token on cue, or drop a chunk upload at byte 8,388,608. A local mock server can do all of that, instantly and the same way every run. The result is a test suite that exercises your hardest code (retry, refresh, resume) in milliseconds.

## Know the limits of the tool

- `wiremock` mocks at the **HTTP level**: status codes, headers, bodies and response delays. It does **not** simulate TCP resets, dropped packets, or a body that drips one byte at a time. For those, put a fault-injecting proxy such as `toxiproxy` in front of the mock, or write a small `axum` or `hyper` server whose response body is a deliberately throttled stream.
- A mock encodes **your beliefs** about the real API. If a belief is wrong, your tests pass and production fails. See "Keeping the mock honest" below.

## Basic setup

```rust
use wiremock::{MockServer, Mock, ResponseTemplate};
use wiremock::matchers::{method, path, header_exists};

#[tokio::test]
async fn retries_after_429_then_succeeds() {
    let server = MockServer::start().await;

    // First two calls are rate limited, then success.
    Mock::given(method("GET")).and(path("/drive/v3/files/abc"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "1"))
        .up_to_n_times(2)
        .mount(&server).await;

    Mock::given(method("GET")).and(path("/drive/v3/files/abc"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id":"abc"})))
        .mount(&server).await;

    let client = DriveClient::new(server.uri()); // base URL must be injectable
    let file = client.get_file("abc").await.unwrap();
    assert_eq!(file.id, "abc");
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}
```

The one design requirement: **the base URL and the token endpoint must be configurable**, so tests can point them at the mock.

## Behaviours to simulate

| Scenario | How |
|---|---|
| Rate limit | 429 or 403 with an error body; `up_to_n_times(n)` then success |
| Retry-After | Add the header and assert the client waits at least that long |
| Expired access token | Return 401 once, then require the refreshed token in `Authorization` via a matcher |
| Revoked refresh token | Token endpoint returns 400 with `{"error":"invalid_grant"}`; assert the account is marked degraded and the daemon keeps serving others |
| Server errors | 500 and 503 sequences; assert capped exponential backoff with jitter |
| Slow response | `ResponseTemplate::set_delay(...)`; assert your timeout fires |
| Malformed body | Truncated JSON; assert a clean error, no panic |

Google's own guidance for quota errors (403 and 429) is to retry with exponential backoff, so your tests should assert that behaviour, including a maximum wait and a maximum attempt count.

## Stateful behaviour: the resumable upload protocol

Static mocks cannot model a protocol that remembers progress. Implement wiremock's `Respond` trait with shared state, such as an `Arc<Mutex<u64>>` holding the committed offset.

The protocol you are modelling (verify against the current Drive docs before relying on details):

1. `POST` with `uploadType=resumable` returns `200` and a session URI in the `Location` header.
2. The client `PUT`s chunks to that URI with `Content-Range: bytes start-end/total`. Chunk sizes are multiples of 256 KiB except the last.
3. For an incomplete upload the server answers `308` with a `Range` header showing what it has stored.
4. After a failure, the client asks for status with an empty body and `Content-Range: bytes */total`, then continues from the reported offset.
5. The final chunk returns `200` or `201` with the file metadata.

Assertions that find real bugs:

- After a 503 on chunk N, the client **queries status** and resumes at the server's offset, not its own guess.
- The client never re-sends bytes the server already committed, and never skips bytes.
- The reassembled content the mock stored hashes identically to the source.

## Chaos mode that stays reproducible

Randomised failures are valuable, but a failure you cannot replay is useless. Drive the chaos from a **seeded RNG** and print the seed on failure:

```rust
let seed = std::env::var("CHAOS_SEED").ok().and_then(|s| s.parse().ok())
    .unwrap_or_else(|| rand::random());
eprintln!("chaos seed = {seed}");
let mut rng = StdRng::seed_from_u64(seed);
```

Then a custom `Respond` implementation picks a fault (429, 503, delay, success) from the RNG. "Survive ten minutes of chaos" is a good soak test, but only if a failing run can be replayed with the same seed.

## Making time cheap

Backoff tests that really sleep make the suite slow. Inject a clock or sleeper into the retry code, or use Tokio's paused time (`#[tokio::test(start_paused = true)]`) so waits complete instantly. Check how wiremock delays interact with paused time in your versions, since mock-side delays may use real time. Keep mock-side delays tiny and test backoff durations through the injected clock.

## Keeping the mock honest

- Record a few real responses (with identifiers scrubbed) and use them as fixtures, so body shapes match reality.
- Add a small **contract test** that runs against the real API using a dedicated throwaway account. Gate it behind an environment variable, run it nightly, never in per-commit CI.
- When the contract test fails, update the mock first, then the client.

## Review checklist

1. Are base URL and token URL injectable?
2. Is each failure mode in the table covered by at least one test?
3. Does the resumable-upload test check offsets and content hash, not just "it finished"?
4. Is every random fault seeded and the seed printed?
5. Do backoff tests avoid real sleeping?
6. Is there a gated contract test against the real service?

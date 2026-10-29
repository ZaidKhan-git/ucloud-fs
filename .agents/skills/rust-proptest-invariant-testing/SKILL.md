---
name: rust-proptest-invariant-testing
description: Property-based testing, state-machine testing and invariant design in Rust using proptest (plus cargo-fuzz and loom where they fit) for path resolvers, chunk splitting and reassembly, offset arithmetic and virtual filesystem logic. Use this skill whenever the user mentions proptest, fuzzing, property tests, invariants, path traversal, Unicode filenames, chunking, striping, unaligned offsets, zero-byte writes, shrinking, or says unit tests keep missing edge cases. Use it even if they only ask "how do I test my path validator" or "how do I know reassembly is correct".
---

# Rust Property-Based Invariant Testing

## The idea

A unit test says "for this input, expect this output". A property test says "for *every* input of this kind, this statement stays true". The computer then generates many inputs, and when one fails it *shrinks* it to a minimal case you can debug. You stop guessing which edge cases exist and start stating what must never break.

The skill is mostly in choosing good **invariants**. A weak invariant ("it doesn't crash") finds little. A strong one ("reassembling split chunks always yields the original bytes") finds real bugs.

## Honest expectations

- `proptest` runs **256 cases per test by default**, not 10,000. Raise it with `PROPTEST_CASES=10000` in CI or per-test config. More cases cost time.
- It generates **random** inputs. It is not coverage-guided and not adversarial. For parsers and binary decoders that must survive hostile input, add `cargo-fuzz` (libFuzzer, coverage-guided). The two tools complement each other.
- It finds bugs in logic you can run deterministically in memory. It does not test concurrency (use `loom`) or real network behaviour (use a mock server).

## Invariants worth writing for a VFS

| Area | Invariant |
|---|---|
| Path normalisation | Normalising twice equals normalising once (idempotent) |
| Path safety | A normalised path never resolves outside the mount root, whatever the input |
| Path equality | Two spellings that the platform treats as the same file map to one key |
| Chunking | `reassemble(split(data, chunk_size)) == data` for any length, including 0 and exact multiples |
| Chunking | Every chunk except the last has exactly `chunk_size` bytes |
| Offsets | For any `(offset, len)`, reading returns exactly the bytes of the reference model, clipped at end of file |
| Hashing | The hash of the reassembled stream equals the hash of the original |
| Routing | Every file maps to exactly one location, and removal leaves no dangling entries |

## Make the code testable first

The biggest lever is a small design choice: **make chunk size a parameter.** In production it might be megabytes. In tests set it to 7 bytes, so every boundary condition is hit by tiny inputs and shrinking produces readable failures.

Also keep the logic under test free of network and clock access. Pure functions in, pure values out.

## Writing strategies (generators)

```rust
use proptest::prelude::*;

fn path_component() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => "[a-zA-Z0-9_ .-]{0,12}",          // ordinary names
        1 => Just("..".to_string()),
        1 => Just(".".to_string()),
        1 => Just("".to_string()),               // empty segment
        1 => any::<String>(),                    // arbitrary Unicode
        1 => prop::sample::select(vec![          // Windows-reserved and awkward names
            "CON", "NUL", "COM1", "trailing.", "trailing ", "a\\b", "x\u{0}y",
        ]).prop_map(String::from),
    ]
}

fn raw_path() -> impl Strategy<Value = String> {
    prop::collection::vec(path_component(), 0..12).prop_map(|v| v.join("/"))
}

proptest! {
    #[test]
    fn normalise_never_escapes_root(p in raw_path()) {
        if let Ok(n) = normalise(&p) {
            prop_assert!(!n.components().any(|c| c == ".."));
            prop_assert!(n.starts_with("/"));
        }
    }

    #[test]
    fn normalise_is_idempotent(p in raw_path()) {
        if let Ok(once) = normalise(&p) {
            prop_assert_eq!(normalise(&once.to_string()).unwrap(), once);
        }
    }

    #[test]
    fn chunk_roundtrip(data in prop::collection::vec(any::<u8>(), 0..4096),
                       chunk in 1usize..64) {
        let parts = split(&data, chunk);
        prop_assert_eq!(reassemble(&parts), data);
    }
}
```

Path edge cases specific to Windows and macOS that your generators should cover:

- Case-insensitive comparison (`File.txt` equals `file.txt`).
- Unicode normalisation (the same visible name as NFC or NFD bytes).
- Reserved device names, trailing dots and spaces, and the 260-character legacy limit.
- Both separators (`/` and `\`), repeated separators, and NUL bytes.
- Names that are not valid UTF-8 or UTF-16. Rust `String` cannot hold them, so use `OsString` at the OS boundary and test conversion explicitly.

## State-machine testing (the strongest tool for a filesystem)

Instead of testing functions one at a time, generate a random **sequence of operations** (create, write at offset, read, truncate, rename, delete) and apply each to two things: your implementation and a trivial in-memory model (a `HashMap<Path, Vec<u8>>`). After each step, assert both agree. When a sequence fails, shrinking removes steps until only the few that matter remain.

Use the `proptest-state-machine` crate, or hand-roll it with a `Vec<Op>` strategy and a loop. The reference model is intentionally naive, because it should be obviously correct.

Operations to include in the generator:

- Writes at unaligned offsets, zero-length writes, writes past end of file (sparse regions must read back as zeros).
- Overlapping writes where the later one wins.
- Truncate to smaller and larger sizes.
- Rename onto an existing name, and rename of a directory with children.

## Working practices

- **Commit the `proptest-regressions/` directory.** Failing seeds are saved there and replayed on every run, so a bug found once stays fixed.
- **Bound the generators.** Keep sizes small (a few KB, not MB) so thousands of cases finish in seconds.
- **No network, no clock, no filesystem** inside a property test. Flaky properties are worse than none.
- **Write the failing property before the fix**, then keep it as a permanent regression test.
- **Use `prop_assume!` sparingly.** Rejecting too many inputs wastes cases. Prefer building the generator so invalid inputs cannot occur.

## Review checklist

1. Is chunk size injectable so tests can use tiny values?
2. Does at least one property cover each row of the invariants table that applies?
3. Do the path generators include `..`, empty segments, Unicode and reserved names?
4. Is there a reference-model state-machine test for the full read/write path?
5. Are regression seeds committed?
6. Is there a `cargo-fuzz` target for any function that parses untrusted bytes?

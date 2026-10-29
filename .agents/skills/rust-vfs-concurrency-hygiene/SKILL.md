---
name: rust-vfs-concurrency-hygiene
description: Deadlock-free concurrency design for Rust daemons that mix OS filesystem threads (FUSE, WinFsp) with Tokio background tasks sharing an in-memory routing table or inode tree. Use this skill whenever the user mentions DashMap, RwLock, Mutex, parking_lot, ArcSwap, lock ordering, deadlock, a frozen mount, a hung Explorer, shared state between sync and async code, block_on, quota counters, delta polling tasks, or an inode/routing table accessed from many threads. Use it even if they only say "my drive freezes forever" or ask which concurrent map to pick.
---

# Rust VFS Concurrency Hygiene

## The shape of the problem

A mounted filesystem has two worlds touching the same data:

- **Foreground OS threads** serving `read`, `readdir`, `write`, `rename`. They are synchronous, latency-sensitive, and the OS waits on them. If one blocks forever, the application (often Explorer or a shell) hangs with it.
- **Background Tokio tasks** doing change polling, token refresh, quota accounting, and retries. They are asynchronous and bursty.

Both read and mutate the routing table. Deadlocks come from two things happening together: a thread holds one lock while wanting another, and nobody agreed on the order. The fix is mostly *design discipline*, not clever primitives.

## Three rules that prevent most hangs

1. **Never do I/O while holding a lock.** Network calls, disk reads and `.await` points can take seconds or never return. Anyone else wanting the lock waits for the same time. Copy what you need, drop the guard, then do the slow work.
2. **Never hold a guard across `.await`.** Standard and `parking_lot` guards are not meant to live across suspension points, and holding one can stall every task that needs it while the executor thread is parked.
3. **Acquire locks in one global order.** If code ever needs two locks, every path takes them in the same sequence.

## Choosing the structure

| Situation | Reach for | Why |
|---|---|---|
| Large map, many independent keys, short operations | `DashMap` | Sharded locking reduces contention |
| Read-mostly snapshot (routing rules, account list) | `arc_swap::ArcSwap<T>` | Readers never block; writers swap a whole new version |
| Counters (bytes used, request counts) | `AtomicU64` | No lock at all |
| Complex state with invariants across fields | One actor task owning the state, reached by a channel | Single-threaded logic cannot deadlock itself |
| Tree where parent and child change together | `parking_lot::RwLock` per inode with strict ordering | Explicit and checkable |

**Correction to a common belief:** `DashMap` is *not* lock-free. It splits the table into shards, each protected by a read-write lock. It reduces contention, but it can still deadlock. Holding a `Ref` or `RefMut` and then calling back into the same map (even for a different key that happens to land in the same shard) can block forever. Treat a `DashMap` guard exactly like a lock guard.

## Safe patterns

### Snapshot, act, validate

```rust
// 1. Take a short look under the lock and clone cheap handles.
let (account, remote_id, version) = {
    let entry = self.routes.get(&ino).ok_or(FsError::NotFound)?;
    (entry.account.clone(), entry.remote_id.clone(), entry.version)
}; // guard dropped here

// 2. Do the slow work with no lock held.
let data = self.client.fetch(&account, &remote_id, range).await?;

// 3. Re-check before committing, in case the entry changed meanwhile.
if let Some(mut e) = self.routes.get_mut(&ino) {
    if e.version != version { return Err(FsError::Retry); }
    e.touch();
}
```

The version number (a generation counter) is cheap optimistic concurrency. It lets you drop the lock during I/O without losing correctness.

### Lock hierarchy for a tree

Write the order in a comment at the top of the module and enforce it in code review:

```
Order: routing_table shard  ->  parent inode  ->  child inode  ->  quota accounting
Rule: never acquire an earlier lock while holding a later one.
```

For operations touching two siblings (such as `rename` across directories), the safe rule is "lock by ascending inode number". Two threads renaming A to B and B to A then lock in the same order and cannot deadlock.

### Bridging sync callbacks and async work

OS threads are not Tokio workers. Keep a runtime `Handle` and call `handle.block_on(fut)` from the callback thread. Never call `block_on` from inside a Tokio worker thread, which panics or stalls the runtime. Better still, give each request a deadline:

```rust
let out = handle.block_on(tokio::time::timeout(Duration::from_secs(30), work()));
match out { Ok(r) => r, Err(_) => Err(FsError::Io) }
```

An operation that times out returns an error. An operation that hangs freezes the application.

## Detecting problems before users do

- **`parking_lot` deadlock detection.** Enable the `deadlock_detection` feature and run a small thread that calls `parking_lot::deadlock::check_deadlock()` every few seconds in debug and staging builds. It reports which threads wait on which locks.
- **`loom`.** Model-check the small concurrent core (the routing table update protocol) by exploring thread interleavings. Keep the tested piece tiny, because the search space explodes.
- **`tokio-console`.** Shows tasks that have not been polled for a long time, a typical sign of a blocked executor.
- **Per-request watchdog.** Log any callback taking longer than a threshold with its operation name and path.
- **Thread sanitizer (nightly).** Catches data races, which are a different bug from deadlocks.

## Review checklist

1. Search for `.await` between a guard acquisition and its drop.
2. Search for any network or disk call inside a lock scope.
3. Confirm one documented lock order, and that every multi-lock path follows it.
4. Confirm no `DashMap` guard is alive while calling a method that touches the same map.
5. Confirm every callback that waits on async work has a timeout.
6. Confirm background tasks hold locks only for in-memory updates.

## Common mistakes

- Using `tokio::sync::Mutex` everywhere to "make it async-safe". It permits holding across `.await`, which invites long critical sections. Restructure so the lock is not needed across the wait.
- Updating quota under the same lock as the routing table, so a slow accounting path blocks all lookups.
- Polling tasks that take a write lock for the whole duration of applying a large delta. Apply in small batches and release between them.
- Assuming reads are harmless. A read lock held by a slow reader can starve a writer on some lock implementations.

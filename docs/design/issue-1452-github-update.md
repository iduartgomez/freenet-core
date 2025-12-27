# Leverage Streaming for Optimal Data Transference

## Problem Statement

Large contract transfers (multi-MB) suffer from three bottlenecks:

1. **Full serialization before send** - `bincode::serialize(&data)` requires complete object in memory
2. **Full reassembly at each hop** - `InboundStream` buffers entire message before returning
3. **No piped forwarding** - Intermediate nodes buffer 100% of data before forwarding
4. **Excessive cloning** - `WrappedState`/`ContractContainer` cloned at every handoff point

**Memory Impact:** 10MB contract traversing 3 hops requires 60MB total (10MB × 2 buffers × 3 nodes) vs ~60KB with piped streaming. With current cloning, serving 3 GETs from cache adds another 30MB.

## Spike Validation ✅

Proof-of-concept in `crates/core/src/transport/streaming_spike.rs`:

| Metric | Result |
|--------|--------|
| In-order delivery buffer | **0 bytes** (100% saved) |
| Realistic reordering buffer | **0.2%** of data (12KB for 5MB) |
| Throughput | **1,338 MB/s** |
| First-fragment latency | **106μs** |

---

## Foundation: Bytes-based State Types

**Critical stdlib change** - Switch `WrappedState` and `ContractContainer` internals from `Vec<u8>` to `bytes::Bytes`:

```rust
// freenet-stdlib changes
use bytes::Bytes;

pub struct WrappedState(Bytes);

impl WrappedState {
    pub fn new(data: impl Into<Bytes>) -> Self { Self(data.into()) }
    pub fn into_bytes(self) -> Bytes { self.0 }
    pub fn as_ref(&self) -> &[u8] { &self.0 }
    pub fn size(&self) -> usize { self.0.len() }
}

impl Clone for WrappedState {
    fn clone(&self) -> Self {
        Self(self.0.clone())  // Refcount bump, NOT memory copy
    }
}

// From Vec<u8> - one allocation, then zero-copy sharing
impl From<Vec<u8>> for WrappedState {
    fn from(v: Vec<u8>) -> Self { Self(Bytes::from(v)) }
}

// From Bytes - zero-copy
impl From<Bytes> for WrappedState {
    fn from(b: Bytes) -> Self { Self(b) }
}
```

### Integration Points Analysis

**Where this eliminates copies:**

| Location | Current | With Bytes |
|----------|---------|------------|
| `state_store.rs:127` - cache get | Full clone | Refcount bump |
| `state_store.rs:97,112` - cache store | Clone for cache + storage | One Bytes, shared |
| `get.rs:1198` - GetResult creation | `state.clone()` full copy | Refcount bump |
| `put.rs:610-611` - state extraction | Clone contract + state | Refcount bumps |
| `update.rs:774` - state conversion | `new_val.clone()` | Refcount bump |
| Multiple consumers (cache + forward) | 2× full copies | 1 allocation, shared |

**Unavoidable copies:**

| Location | Reason |
|----------|--------|
| `redb.rs:176` - DB read | Transaction scope ends, must own data |
| Network receive | Incoming bytes → `Bytes::from(vec)` (one alloc) |
| WASM boundary | Linear memory copy (WASM limitation) |

**Memory comparison:**
```
Scenario: Cache 10MB state, serve 3 GET requests

Current (Vec<u8>):  10MB cache + 30MB clones = 40MB
With Bytes:         10MB cache + 0 copies    = 10MB
```

---

## Stream Consumption Abstraction

Unified `Stream` trait API for all consumption patterns:

```rust
use bytes::Bytes;
use futures::Stream;

/// Incoming stream of contract/state data
pub struct InboundStream {
    store: Arc<FragmentStore>,
    position: AtomicU64,
}

impl Stream for InboundStream {
    type Item = Bytes;
    // poll_next yields fragments as they become contiguous
}

impl InboundStream {
    /// Collect into WrappedState - zero-copy with Bytes
    pub async fn into_state(self) -> WrappedState {
        WrappedState::from(self.collect_bytes().await)
    }

    /// Collect into ContractContainer
    pub async fn into_contract(self) -> Result<ContractContainer> {
        ContractContainer::try_from(self.collect_bytes().await)
    }

    /// Fork for multiple consumers (forward + store)
    pub fn fork(&self) -> Self {
        Self {
            store: Arc::clone(&self.store),
            position: AtomicU64::new(0),
        }
    }

    async fn collect_bytes(self) -> Bytes {
        use futures::StreamExt;
        let mut buf = BytesMut::with_capacity(self.store.total_size as usize);
        let mut stream = self;
        while let Some(chunk) = stream.next().await {
            buf.extend_from_slice(&chunk);
        }
        buf.freeze()  // BytesMut -> Bytes, zero-copy
    }
}
```

### Three Consumption Patterns

```rust
async fn handle_contract_stream(
    stream: InboundStream,
    should_forward: bool,
    should_store: bool,
    next_hops: &[PeerConnection],
    state_store: &StateStore,
    key: &ContractKey,
) -> Result<()> {

    match (should_forward, should_store) {
        // Forward only - stream through, never collect
        (true, false) => {
            forward_stream(stream, next_hops).await
        }

        // Store only - collect into WrappedState
        (false, true) => {
            let state: WrappedState = stream.into_state().await;
            state_store.store(key, state).await
        }

        // Both - fork stream, parallel consumption
        (true, true) => {
            let forward_stream = stream.fork();
            let store_stream = stream;

            let (_, state) = tokio::join!(
                forward_stream(forward_stream, next_hops),
                store_stream.into_state()
            );

            state_store.store(key, &state).await
        }

        (false, false) => Ok(()),
    }
}
```

**Memory profile:**
```
Forward only:  O(out_of_order_buffer) ≈ few KB
Store only:    O(contract_size) - need full state for WrappedState
Both:          O(contract_size) - one copy, streamed forward
```

---

## Implementation Phases

### Phase 1: Lock-Free StreamBuffer

Pre-allocated buffer with atomic fragment slots:

```rust
use std::sync::OnceLock;
use bytes::Bytes;

pub struct StreamBuffer {
    /// Each slot holds one fragment - OnceLock for lock-free write-once
    fragments: Box<[OnceLock<Bytes>]>,
    /// Atomic frontier tracking
    contiguous_fragments: AtomicU32,
    total_fragments: u32,
}

impl StreamBuffer {
    pub fn new(total_size: u64, fragment_size: usize) -> Self {
        let n = total_size.div_ceil(fragment_size as u64) as usize;
        Self {
            fragments: (0..n).map(|_| OnceLock::new()).collect(),
            contiguous_fragments: AtomicU32::new(0),
            total_fragments: n as u32,
        }
    }

    /// Insert fragment - completely lock-free
    pub fn insert(&self, fragment_num: u32, data: Bytes) {
        let idx = (fragment_num - 1) as usize;
        // OnceLock::set is lock-free CAS - duplicates are no-ops
        let _ = self.fragments[idx].set(data);
        self.advance_frontier();
    }

    fn advance_frontier(&self) {
        loop {
            let current = self.contiguous_fragments.load(Ordering::Acquire);
            if current >= self.total_fragments {
                break;
            }
            if self.fragments[current as usize].get().is_some() {
                match self.contiguous_fragments.compare_exchange_weak(
                    current, current + 1,
                    Ordering::AcqRel, Ordering::Relaxed,
                ) {
                    Ok(_) => continue,
                    Err(_) => continue,
                }
            } else {
                break;
            }
        }
    }

    /// Iterate available fragments - no locks
    pub fn iter(&self) -> impl Iterator<Item = &Bytes> {
        let n = self.contiguous_fragments.load(Ordering::Acquire) as usize;
        self.fragments[..n].iter().filter_map(|s| s.get())
    }
}
```

**Why OnceLock:**
- `set()` is a single atomic CAS - no mutex
- `get()` is a single atomic load - no contention
- Duplicate fragments are automatically no-ops
- Concurrent writers to different slots never block each other

**Key tasks:**
- [ ] Implement `StreamBuffer` with `OnceLock<Bytes>` slots
- [ ] Pre-allocate based on `total_size` from stream header
- [ ] `StreamRegistry` for transport→operations handoff
- [ ] `InboundStream` implementing `futures::Stream`

### Phase 2: Piped Forwarding

Enable intermediate nodes to forward without full reassembly:

```rust
pub struct PipedStream {
    stream_id: StreamId,
    next_to_forward: u32,
    out_of_order: BTreeMap<u32, Bytes>,
    targets: Vec<PeerConnection>,

    pub async fn pipe_fragment(&mut self, frag_num: u32, data: Bytes) -> bool;
}
```

**Key tasks:**
- [ ] `send_fragment()` low-level API on PeerConnection
- [ ] Bounded backpressure with semaphores per target
- [ ] Memory pressure limits (max buffered fragments)

### Phase 3: Message Layer Integration

Add streaming variants for large transfers:

```rust
pub enum PutMsg {
    RequestPut { contract: ContractContainer, ... },  // Existing

    RequestPutStreaming {  // New
        id: Transaction,
        stream_id: StreamId,
        contract_key: ContractKey,
        total_size: u64,
        htl: usize,
        target: PeerKeyLocation,
    },
}
```

**Key tasks:**
- [ ] Extend `PutOp`/`GetOp` state machines with stream tracking
- [ ] Handle race conditions (stream before/after metadata)
- [ ] Two-phase message parsing (header-only for routing)

### Phase 4: stdlib Bytes Migration

**Key tasks:**
- [ ] Change `WrappedState` internal from `Vec<u8>` to `Bytes`
- [ ] Change `ContractContainer` code storage to `Bytes`
- [ ] Update `State` type similarly
- [ ] Ensure serde compatibility

### Phase 5: Core Integration

**StateStore changes:**
```rust
pub struct StateStore<S: StateStorage> {
    store: S,
    state_mem_cache: AsyncCache<ContractKey, Bytes>,  // was WrappedState
}

impl<S: StateStorage> StateStore<S> {
    pub async fn get(&self, key: &ContractKey) -> Result<WrappedState> {
        if let Some(v) = self.state_mem_cache.get(key).await {
            return Ok(WrappedState::from(v.value().clone())); // Refcount bump
        }
        // ... fetch from persistent store
    }
}
```

**Key tasks:**
- [ ] Update `StateStore` to cache `Bytes` directly
- [ ] Update storage backends (`redb.rs`, `sqlite.rs`)
- [ ] Update operation state machines to use streaming types
- [ ] Remove redundant clones in `get.rs`, `put.rs`, `update.rs`

### Phase 6: Gradual Rollout

1. **Shadow mode** - Run both paths, compare results
2. **Opt-in** - `--streaming-transport` flag
3. **Default on** - Streaming enabled by default
4. **Cleanup** - Remove legacy code path

---

## Throughput Footguns

| Footgun | Problem | Mitigation |
|---------|---------|------------|
| Lock contention | RwLock on every fragment | Lock-free `OnceLock<Bytes>` slots |
| Memory allocation | O(fragments) reallocs | Pre-allocate from `total_size` |
| Notification overhead | `Notify` on every fragment | Batch at 64KB thresholds |
| Contiguous calculation | O(n²) total | Incremental frontier tracking |
| LEDBAT waiting | Busy loop when cwnd full | Async condition variable |
| Backpressure | Unbounded queue to slow peers | Bounded semaphores |
| Excessive cloning | Vec<u8> clones at each handoff | Bytes refcounting |
| Deserialization | Full parse before routing | Two-phase header/body |

---

## Stream Lifecycle

```
SENDER                      INTERMEDIATE                   TARGET
   │                              │                           │
   ├─► Start stream fragments ────┼───────────────────────────│
   │                              │                           │
   ├─► RequestPutStreaming ──────►│                           │
   │   (metadata only)            │                           │
   │                              ├─► Claim stream            │
   │                              │                           │
   │   [Fragments arrive]         ├─► Create PipedStream      │
   │                              │                           │
   │                              ├─► Forward fragments ──────┼──►
   │                              │   (zero-copy Bytes)       │
   │                              │                           │
   │                              ├─► RequestPutStreaming ────┼──►
   │                              │   (new stream_id)         │
   │                              │                           │
   │                              │                           ├─► stream.into_state()
   │                              │                           ├─► store WrappedState
   │                              │                           │
   │                              │◄─────────────────────────├─► ResponsePut
   │◄────────────────────────────│                           │
```

**Race condition handling:**
- Stream arrives before metadata → Store in `orphan_streams`, claim later
- Metadata arrives before stream → Register waiter with oneshot channel

---

## Configuration

```rust
pub struct TransportConfig {
    pub streaming_enabled: bool,              // Default: false initially
    pub streaming_threshold_bytes: usize,     // Default: 64KB
    pub piped_forwarding: bool,               // Default: false initially
}
```

---

## Success Metrics

- Memory reduction: **100×** for forwarding large contracts
- Clone elimination: **4×** memory reduction for cached state serving
- First-byte latency: **< 1ms** (vs full transfer time)
- Zero regressions for small messages (< 64KB)
- Backward compatible with older nodes

---

## Key Files

| File | Purpose |
|------|---------|
| `crates/core/src/transport/streaming_spike.rs` | Proof-of-concept |
| `crates/core/src/wasm_runtime/state_store.rs` | State caching (lines 81-131) |
| `crates/core/src/operations/get.rs` | GET flow (clones at 1198-1200) |
| `crates/core/src/operations/put.rs` | PUT flow (clones at 610-612) |
| `crates/core/src/contract/storages/redb.rs` | Persistent storage |
| `freenet-stdlib` | WrappedState/ContractContainer types |

---

## Dependencies

- [x] `Bytes`-based zero-copy fragmentation in transport
- [x] LEDBAT congestion control
- [x] TokenBucket rate limiting
- [ ] stdlib `Bytes` migration (Phase 4)
- [ ] Phase 1-6 implementation

## Related

- Spike branch: `claude/review-stream-transport-AzSg3`
- Design docs: `docs/design/streaming-transport-deep-dive.md`

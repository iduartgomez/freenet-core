# Issue #1452: Streaming Transport Implementation Plan

## Executive Summary

This document provides a detailed implementation plan for leveraging streaming to optimize large data transfers in Freenet. A proof-of-concept spike has validated the core concepts, and this plan outlines the phases needed to integrate streaming into production.

**Key Benefits:**
- **Memory reduction**: From 100% buffering (current) to ~0.1% for typical network reordering
- **Latency improvement**: First-fragment availability in ~25µs vs waiting for complete message
- **Forwarding efficiency**: Intermediate nodes can pipe streams without full reassembly

**Spike Validation Results (10MB data, 7688 fragments):**

| Metric | Lock-free (OnceLock) | RwLock | Speedup |
|--------|---------------------|--------|---------|
| Insert throughput | **2,235 MB/s** | 23 MB/s | **96×** |
| First-fragment latency | **25µs** | 103µs | **4×** |

| Memory Scenario | Buffer Required |
|-----------------|-----------------|
| In-order piped forwarding | **0 bytes** |
| Realistic reordering (chunks of 10) | **12 KB** (0.1%) |
| Worst case (reverse order) | 10 MB (100%) |

| Clone Operation (10MB, 100 iterations) | Time | Speedup |
|----------------------------------------|------|---------|
| **Bytes::clone** (refcount) | 14µs | baseline |
| Vec::clone (full copy) | 905ms | **63,000×** |

---

## Current Architecture Analysis

### What's Already Optimized ✅

The transport layer has already adopted `Bytes` for zero-copy fragmentation:

```rust
// outbound_stream.rs - already using Bytes
pub(crate) type SerializedStream = Bytes;

// Zero-copy fragmentation using Bytes::slice()
let fragment = stream_to_send.slice(..MAX_DATA_SIZE);
stream_to_send = stream_to_send.slice(MAX_DATA_SIZE..);
```

Additionally:
- LEDBAT congestion control (RFC 6817) is in place
- TokenBucket rate limiting smooths packet pacing
- GlobalBandwidthManager provides fair sharing across connections

### Remaining Bottlenecks 🔴

1. **Full Serialization Before Send** (`peer_connection.rs:346`)
   ```rust
   pub async fn send<T>(&mut self, data: T) -> Result {
       let data = bincode::serialize(&data)?;  // ← Must have complete object
       if data.len() > MAX_DATA_SIZE {
           self.outbound_stream(data).await;
       }
   }
   ```

2. **Full Reassembly Before Receive** (`inbound_stream.rs`)
   ```rust
   pub(super) async fn recv_stream(...) -> Result<(StreamId, Vec<u8>), StreamId> {
       // Returns only when stream is COMPLETE
       while let Ok((fragment_number, payload)) = receiver.recv_async().await {
           if let Some(msg) = stream.push_fragment(fragment_number, payload) {
               return Ok((stream_id, msg));  // ← Only returns complete message
           }
       }
   }
   ```

3. **No Piped Forwarding**
   - Intermediate nodes must: receive all → deserialize → re-serialize → send
   - Memory pressure grows linearly with contract size at each hop

### Data Flow Comparison

```
CURRENT:
┌────────────┐     ┌─────────────────────┐     ┌────────────┐
│  Sender    │────►│  Intermediate Node  │────►│  Receiver  │
└────────────┘     └─────────────────────┘     └────────────┘
   serialize         reassemble 100%             reassemble
   all data ─────►   then re-serialize ─────►   100%
   (10MB)            (10MB × 2)                  (10MB)

PROPOSED:
┌────────────┐     ┌─────────────────────┐     ┌────────────┐
│  Sender    │────►│  Intermediate Node  │────►│  Receiver  │
└────────────┘     └─────────────────────┘     └────────────┘
   stream            pipe fragments              reassemble
   chunks ──────►   (~0.2% buffer) ──────────►  on demand
```

---

## Implementation Plan

### Phase 0: Foundation (Already Complete ✅)

- [x] `Bytes`-based zero-copy fragmentation for outbound streams
- [x] LEDBAT congestion control
- [x] TokenBucket rate limiting
- [x] Spike validation of StreamHandle and PipedStream concepts

### Phase 1: StreamHandle Infrastructure

**Goal:** Enable incremental access to incoming streams without waiting for completion.

**Files to modify:**
- `crates/core/src/transport/peer_connection/inbound_stream.rs`
- `crates/core/src/transport/peer_connection.rs`

**New types (lock-free design validated in spike):**

```rust
use std::sync::OnceLock;
use bytes::Bytes;

/// Lock-free buffer using OnceLock for each fragment slot.
/// 96× faster than RwLock approach (2,235 MB/s vs 23 MB/s).
pub struct LockFreeStreamBuffer {
    /// Each slot holds one fragment - OnceLock for lock-free write-once
    fragments: Box<[OnceLock<Bytes>]>,
    /// Pre-computed fragment sizes for proper byte counting
    fragment_sizes: Box<[usize]>,
    /// Total expected size
    total_size: u64,
    /// Number of fragments
    total_fragments: u32,
    /// Contiguous fragment count (atomic frontier)
    contiguous_fragments: AtomicU32,
    /// Notify waiters when new contiguous data available
    data_available: Notify,
}

impl LockFreeStreamBuffer {
    /// Insert fragment - completely lock-free using CAS
    pub fn insert(&self, fragment_num: u32, data: Bytes) -> bool {
        let idx = (fragment_num - 1) as usize;
        // OnceLock::set is lock-free CAS - duplicates are no-ops
        let _ = self.fragments[idx].set(data);
        self.advance_frontier();
        self.is_complete()
    }

    /// Iterate available fragments - no locks needed
    pub fn iter_contiguous(&self) -> impl Iterator<Item = &Bytes>;
}

/// Consumer handle implementing futures::Stream for async iteration.
/// Multiple consumers can read from the same buffer independently.
pub struct InboundStream {
    buffer: Arc<LockFreeStreamBuffer>,
    current_fragment: u32,
}

impl futures::Stream for InboundStream {
    type Item = Bytes;
    // poll_next yields fragments as they become contiguous
}

impl InboundStream {
    /// Fork into independent consumer with its own position
    pub fn fork(&self) -> Self;

    /// Collect all data into Bytes (waits for completion)
    pub async fn collect_all(self) -> Bytes;
}
```

**New PeerConnection API:**

```rust
impl PeerConnection {
    /// Receive a stream handle (returns immediately when first fragment arrives)
    pub async fn recv_stream_handle(&mut self) -> Result<StreamHandle>;

    /// Legacy: Receive complete message (calls recv_stream_handle().read_all())
    pub async fn recv(&mut self) -> Result<SerializedMessage>;
}
```

**Tasks:**
- [x] Implement `LockFreeStreamBuffer` with `OnceLock<Bytes>` slots *(done in spike)*
- [x] Implement `InboundStream` with `futures::Stream` trait *(done in spike)*
- [x] Unit tests for out-of-order fragment handling *(done in spike)*
- [x] Benchmarks comparing lock-free vs RwLock (96× speedup) *(done in spike)*
- [ ] Port spike implementation to production code
- [ ] Add `recv_stream_handle()` to PeerConnection
- [ ] Update existing `recv()` to use InboundStream internally
- [ ] Integration with `StreamRegistry` for transport→operations handoff

### Phase 2: Piped Streams for Forwarding

**Goal:** Enable intermediate nodes to forward streams without full reassembly.

**New types:**

```rust
/// Forwards stream fragments to target connections without full reassembly.
pub struct PipedStream {
    stream_id: StreamId,
    total_size: u64,
    next_to_forward: u32,
    out_of_order: BTreeMap<u32, Bytes>,
    targets: Vec<PeerConnection>,
    bytes_forwarded: u64,
    bytes_buffered: u64,
    max_buffer_bytes: usize,  // Memory pressure limit
}

impl PipedStream {
    /// Process an incoming fragment - forward immediately if in-order,
    /// buffer if out-of-order. Returns true if stream is complete.
    pub async fn pipe_fragment(&mut self, frag_num: u32, data: Bytes) -> Result<bool>;
}
```

**New PeerConnection API:**

```rust
impl PeerConnection {
    /// Send a single fragment (low-level API for piping)
    pub async fn send_fragment(
        &mut self,
        stream_id: StreamId,
        fragment_num: u32,
        data: Bytes,
    ) -> Result<()>;

    /// Create a piped stream that forwards to targets
    pub fn pipe_stream(
        &mut self,
        stream: StreamHandle,
        targets: Vec<PeerConnection>,
    ) -> PipedStream;
}
```

**Tasks:**
- [x] Implement `PipedStream` with out-of-order buffering *(done in spike)*
- [x] Forward fragments immediately when in-order *(done in spike)*
- [x] Tests for worst-case (reverse order) delivery *(done in spike)*
- [x] Benchmarks for piped forwarding memory usage *(done in spike)*
- [ ] Port spike implementation to production code
- [ ] Add `send_fragment()` to PeerConnection
- [ ] Add `pipe_stream()` factory method
- [ ] Add memory pressure management (max buffered fragments)
- [ ] Add metrics for bytes_buffered / bytes_forwarded

### Phase 3: Message Layer Integration

**Goal:** Add streaming variants to network messages for large transfers.

**Files to modify:**
- `crates/core/src/message.rs`
- `crates/core/src/operations/put.rs`
- `crates/core/src/operations/get.rs`

**New message variants:**

```rust
#[derive(Serialize, Deserialize)]
pub enum PutMsg {
    // Existing: contains full contract data (keep for small contracts)
    RequestPut { ... },

    // New: contract data is streamed separately
    RequestPutStreaming {
        id: Transaction,
        contract_stream_id: StreamId,
        contract_key: ContractKey,
        contract_size: u64,
        value_size: u64,
        htl: usize,
        target: PeerKeyLocation,
    },
}

#[derive(Serialize, Deserialize)]
pub enum GetMsg {
    // Existing: returns full contract
    Response { ... },

    // New: contract will be streamed
    ResponseStreaming {
        id: Transaction,
        contract_stream_id: StreamId,
        contract_size: u64,
    },
}
```

**Streaming threshold configuration:**

```rust
pub struct TransportConfig {
    /// Enable stream-based transport for large messages
    pub streaming_enabled: bool,

    /// Size threshold above which to use streaming (default: 64KB)
    pub streaming_threshold_bytes: usize,

    /// Enable piped forwarding (forward without full reassembly)
    pub piped_forwarding: bool,
}
```

**Tasks:**
- [ ] Add RequestPutStreaming / ResponseStreaming message variants
- [ ] Add streaming_threshold config option
- [ ] Implement auto-selection between message and streaming mode
- [ ] Update PUT operation to support streaming mode
- [ ] Update GET operation to support streaming mode
- [ ] Add integration tests for streaming puts/gets

### Phase 4: Streaming Serialization

**Goal:** Enable incremental serialization for large objects like ContractContainer.

**New trait:**

```rust
/// A streaming serializer that yields fragments as they're ready
pub trait StreamingSerialize {
    /// Start serialization, returning expected total size
    fn start_serialize(&self) -> (u64, StreamingSerializer);
}

pub struct StreamingSerializer {
    fragment_rx: mpsc::Receiver<Bytes>,
}

impl StreamingSerialize for ContractContainer {
    fn start_serialize(&self) -> (u64, StreamingSerializer) {
        let (tx, rx) = mpsc::channel(16);
        let total_size = self.data().len() as u64 + header_overhead();

        let data = self.data().clone();
        tokio::spawn(async move {
            tx.send(serialize_header()).await;
            for chunk in data.chunks(MAX_FRAGMENT_SIZE) {
                tx.send(Bytes::copy_from_slice(chunk)).await;
            }
        });

        (total_size, StreamingSerializer { fragment_rx: rx })
    }
}
```

**New PeerConnection API:**

```rust
impl PeerConnection {
    /// Send data using streaming serialization
    pub async fn send_streaming<T: StreamingSerialize>(&mut self, data: &T) -> Result<StreamId>;

    /// Auto-select message or streaming based on size
    pub async fn send_auto<T: Serialize + StreamingSerialize>(&mut self, data: &T) -> Result<()>;
}
```

**Tasks:**
- [ ] Define StreamingSerialize trait
- [ ] Implement for ContractContainer
- [ ] Implement for WrappedState
- [ ] Add send_streaming() to PeerConnection
- [ ] Add send_auto() with size-based selection
- [ ] Benchmark serialization overhead

### Phase 5: Capability Negotiation

**Goal:** Ensure backward compatibility with nodes that don't support streaming.

**During handshake:**

```rust
#[derive(Serialize, Deserialize)]
struct PeerCapabilities {
    protocol_version: u32,
    streaming_supported: bool,
    piped_forwarding_supported: bool,
}

fn negotiate_mode(local: &TransportConfig, remote: &PeerCapabilities) -> TransportMode {
    if local.streaming_enabled && remote.streaming_supported {
        if local.piped_forwarding && remote.piped_forwarding_supported {
            TransportMode::StreamingPiped
        } else {
            TransportMode::StreamingReassemble
        }
    } else {
        TransportMode::Traditional
    }
}
```

**Tasks:**
- [ ] Add PeerCapabilities to handshake protocol
- [ ] Store negotiated mode per connection
- [ ] Gate streaming features behind capability check
- [ ] Add protocol version bump for streaming support

### Phase 6: Rollout and Monitoring

**Feature flag strategy:**

```bash
# Environment variables for gradual rollout
FREENET_STREAMING=true              # Enable streaming transport
FREENET_STREAMING_THRESHOLD=65536   # Size threshold in bytes
FREENET_PIPED_FORWARDING=true       # Enable piped forwarding
FREENET_STREAMING_SHADOW=true       # Shadow mode for validation
```

**Shadow mode for validation:**

```rust
async fn send_message(&mut self, data: &[u8]) -> Result<()> {
    if config.streaming_shadow_mode && data.len() > config.streaming_threshold {
        // Run both, compare, log differences, return traditional result
        let (trad, stream) = tokio::join!(
            self.send_traditional(data),
            self.send_streaming(data)
        );
        if trad != stream {
            tracing::warn!("Streaming/traditional mismatch");
            metrics::increment!("streaming_mismatches");
        }
        return trad;
    }
    // Normal path...
}
```

**Metrics to track:**

```
streaming_messages_total{mode="streaming|traditional"}
streaming_bytes_total{mode="streaming|traditional"}
streaming_latency_seconds{phase="first_fragment|complete"}
streaming_buffer_bytes_max
streaming_errors_total{type="timeout|buffer_overflow|mismatch"}
piped_streams_total
piped_buffer_bytes_max
```

**Tasks:**
- [ ] Add feature flag configuration
- [ ] Implement shadow mode for A/B validation
- [ ] Add Prometheus metrics
- [ ] Create Grafana dashboard for streaming health
- [ ] Document rollout procedure

---

## Rollout Phases

| Phase | Config | Behavior | Exit Criteria |
|-------|--------|----------|---------------|
| **Shadow** | `streaming_shadow_mode=true` | Both paths run, results compared, traditional used | Zero discrepancies over 1 week |
| **Opt-In** | `streaming_enabled=false` (default) | Users opt-in via CLI | Positive feedback, no regressions |
| **Default On** | `streaming_enabled=true` | Streaming by default, opt-out available | Stable 2-3 releases |
| **Legacy Removal** | N/A | Remove traditional code path | After Phase 3 stable 2-3 releases |

---

## Memory Pressure Management

```rust
struct StreamConfig {
    /// Max fragments to buffer for out-of-order arrival
    max_buffered_fragments: usize,        // Default: 1000 (~1.3MB)

    /// Max total bytes in flight per stream
    max_stream_buffer_bytes: usize,       // Default: 10MB

    /// Max concurrent streams per connection
    max_concurrent_streams: usize,        // Default: 10
}
```

When limits are exceeded:
1. Log warning with stream ID and current usage
2. Apply backpressure (pause receiving new fragments)
3. If sustained, close stream with `BufferOverflow` error

---

## Error Handling

```rust
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("stream timed out waiting for fragments")]
    Timeout,

    #[error("buffer overflow: {buffered} bytes exceeds limit {limit}")]
    BufferOverflow { buffered: usize, limit: usize },

    #[error("fragment {fragment} out of expected range [1, {max}]")]
    InvalidFragment { fragment: u32, max: u32 },

    #[error("connection closed mid-stream at fragment {last_received}/{total}")]
    ConnectionLost { last_received: u32, total: u32 },

    #[error("stream verification failed: expected {expected:?}, got {actual:?}")]
    VerificationFailed { expected: [u8; 32], actual: [u8; 32] },
}
```

---

## Open Questions

1. **Fragment ordering guarantees**: Should piped streams require strict ordering, or allow reordering with larger buffers?
   - *Recommendation*: Allow reordering with configurable buffer limits

2. **Partial stream cancellation**: How to handle streams abandoned mid-transfer?
   - *Recommendation*: Timeout + explicit cancel message

3. **Multiplexing**: Should multiple streams share fragment-level interleaving?
   - *Recommendation*: Separate logical channels per stream (simpler, current approach)

4. **Encryption boundaries**: Current encryption is per-packet. Does streaming change nonce management?
   - *Analysis needed*: Current approach should work as each fragment is a separate packet

5. **Serialization framework**: Should we migrate from bincode to a zero-copy framework?
   - *Recommendation*: Evaluate rkyv for zero-copy deserialization (see section below)

---

## Zero-Copy Serialization Alternatives

Migrating from bincode to a zero-copy framework could eliminate the serialization bottleneck entirely.

**Note:** `rkyv` and `zerocopy` appear in `Cargo.lock` as transitive dependencies (via wasmer and ahash) but are not directly used by freenet-core. `flatbuffers` is used for topology monitoring messages (`schemas/flatbuffers/topology_generated.rs`) but not for core protocol serialization. Adding direct zero-copy serialization for contract/state data would be a new capability.

### Current State: bincode

```rust
// bincode requires complete object → allocates new Vec<u8>
let data = bincode::serialize(&contract)?;  // Allocation + copy
// Later: bincode::deserialize(&data)?      // Another allocation + copy
```

**Limitations:**
- Must have complete object in memory before serialization
- Produces `Vec<u8>` requiring allocation
- Deserialization copies data again

### Option 1: rkyv (Recommended for Investigation)

[rkyv](https://github.com/rkyv/rkyv) provides zero-copy deserialization - archived data can be accessed directly from the buffer.

```rust
use rkyv::{Archive, Deserialize, Serialize};

#[derive(Archive, Serialize, Deserialize)]
struct ContractData {
    code: Vec<u8>,
    state: Vec<u8>,
}

// Serialization: still requires complete object, but...
let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&contract)?;

// Deserialization: ZERO-COPY - access archived data directly from buffer
let archived = rkyv::access::<ArchivedContractData, rkyv::rancor::Error>(&bytes)?;
// `archived.code` points directly into `bytes` - no allocation!

// Can also fully deserialize if needed
let contract: ContractData = archived.deserialize(&mut rkyv::Infallible)?;
```

**Benefits:**
- Zero-copy deserialization from StreamBuffer
- Archived data can be accessed while stream is still arriving (for known-offset fields)
- Mature library with good documentation

**Trade-offs:**
- Archived format is different from live format (need `Archived*` types)
- Requires `#[derive(Archive)]` on all serialized types
- Cross-version compatibility needs care

### Option 2: flatbuffers

Already used in the codebase. Good for structured data with schema.

```rust
// Define schema in .fbs file
table Contract {
    code: [ubyte];
    state: [ubyte];
}

// Access without deserialization
let contract = root_as_contract(&buffer)?;
let code: &[u8] = contract.code().unwrap();  // Zero-copy slice
```

**Benefits:**
- Schema-based with code generation
- Zero-copy access to nested data
- Good tooling and documentation

**Trade-offs:**
- Requires .fbs schema files
- Generated code can be verbose
- Schema evolution needs planning

### Option 3: zerocopy (For Simple Structs)

Best for fixed-size, repr(C) structs.

```rust
use zerocopy::{FromBytes, AsBytes};

#[derive(FromBytes, AsBytes)]
#[repr(C)]
struct PacketHeader {
    stream_id: u32,
    fragment_num: u32,
    total_size: u64,
}

// Zero-copy parsing from buffer
let header = PacketHeader::read_from_prefix(&buffer)?;
```

**Benefits:**
- Simplest API
- No schema or codegen
- Truly zero overhead

**Trade-offs:**
- Only works for fixed-size, repr(C) types
- No variable-length data (strings, vecs)
- Platform-dependent (endianness)

### Recommendation

**Phase 1-4**: Keep bincode for protocol messages, but optimize the hot path:
- Use `Bytes` throughout to avoid copies after initial serialization
- StreamHandle provides zero-copy access to received data

**Future Phase**: Evaluate rkyv migration for ContractContainer/WrappedState:
- These are the largest serialized objects
- Zero-copy deserialization would eliminate receiver-side allocation
- Could access contract metadata before full stream arrives

**Migration path:**
1. Add rkyv derives alongside existing Serialize/Deserialize
2. Add capability flag for rkyv-serialized contracts
3. Support both formats during transition
4. Deprecate bincode format after network upgrade

### Integration with Streaming

With rkyv, the streaming receiver can:
```rust
impl StreamHandle {
    /// Access archived data directly from buffer (zero-copy)
    pub fn access_archived<T: Archive>(&self) -> Result<&T::Archived> {
        let data = self.buffer.get_contiguous().await;
        rkyv::access::<T::Archived, _>(&data)
    }

    /// For partial access - read header while stream continues
    pub fn peek_header<H: Archive>(&self, header_size: usize) -> Option<&H::Archived> {
        if self.available_bytes() >= header_size {
            // Safe: we have enough contiguous bytes
            rkyv::access::<H::Archived, _>(&self.buffer.data[..header_size]).ok()
        } else {
            None
        }
    }
}
```

---

## References

- Spike implementation: `crates/core/src/transport/streaming_spike.rs`
- Design document: `docs/design/stream-based-transport.md`
- Current transport: `crates/core/src/transport/`
- Related PR: Zero-copy optimization #2361

---

## Checklist for Implementation

### Phase 1 Checklist
- [x] LockFreeStreamBuffer with OnceLock<Bytes> slots *(spike)*
- [x] InboundStream with futures::Stream trait *(spike)*
- [x] Notification system for data arrival *(spike)*
- [x] Unit tests for buffer management *(spike)*
- [x] Memory usage benchmarks (96× speedup) *(spike)*
- [ ] Port to production code
- [ ] Integration with PeerConnection

### Phase 2 Checklist
- [x] PipedStream implementation *(spike)*
- [x] Out-of-order fragment buffering *(spike)*
- [x] Integration tests for piped forwarding *(spike)*
- [ ] Port to production code
- [ ] Memory pressure management
- [ ] send_fragment() low-level API

### Phase 3 Checklist
- [ ] Streaming message variants
- [ ] Streaming threshold configuration
- [ ] PUT operation streaming support
- [ ] GET operation streaming support
- [ ] Backwards compatibility tests

### Phase 4 Checklist
- [ ] StreamingSerialize trait
- [ ] ContractContainer implementation
- [ ] WrappedState implementation
- [ ] Serialization benchmarks

### Phase 5 Checklist
- [ ] PeerCapabilities in handshake
- [ ] Mode negotiation logic
- [ ] Protocol version bump
- [ ] Capability-gated feature access

### Phase 6 Checklist
- [ ] Feature flag configuration
- [ ] Shadow mode implementation
- [ ] Prometheus metrics
- [ ] Grafana dashboard
- [ ] Rollout documentation

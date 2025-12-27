# Issue #1452: Streaming Transport Implementation Plan

## Executive Summary

This document provides a detailed implementation plan for leveraging streaming to optimize large data transfers in Freenet. A proof-of-concept spike has validated the core concepts, and this plan outlines the phases needed to integrate streaming into production.

**Key Benefits:**
- **Memory reduction**: From 100% buffering (current) to ~0.2% for typical network reordering
- **Latency improvement**: First-fragment availability in ~106µs vs waiting for complete message
- **Forwarding efficiency**: Intermediate nodes can pipe streams without full reassembly

**Spike Validation Results:**
- In-order delivery: 0 bytes buffered (100% memory saved)
- Realistic reordering: 0.2% of data buffered (12KB for 5MB transfer)
- Throughput: 1,338 MB/s
- First-fragment latency: 106µs

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

**New types:**

```rust
/// Shared buffer that accumulates incoming stream fragments.
/// Designed for zero-copy access via `Bytes`.
pub struct StreamBuffer {
    /// The actual data buffer
    data: RwLock<BytesMut>,
    /// Total expected size (from first fragment)
    total_size: AtomicU64,
    /// Contiguous bytes available from start (for ordered reading)
    contiguous_bytes: AtomicU64,
    /// Track which fragments we've received (for gap detection)
    received_fragments: RwLock<Vec<bool>>,
    /// Notify waiters when new data arrives
    data_available: Notify,
}

/// A handle to a stream being received. Can be cloned and shared
/// without copying the underlying data (uses Arc + Bytes).
#[derive(Clone)]
pub struct StreamHandle {
    pub stream_id: StreamId,
    buffer: Arc<StreamBuffer>,
    read_position: Arc<AtomicU64>,
}

impl StreamHandle {
    /// Read available data without blocking
    pub async fn read_available(&self) -> Bytes;

    /// Async read that waits for more data
    pub async fn read(&self, max_bytes: usize) -> Bytes;

    /// Read all data (waits for completion)
    pub async fn read_all(&self) -> Bytes;

    /// Check if stream is complete
    pub fn is_complete(&self) -> bool;

    /// Get a shared handle (cheap Arc clone, fresh read position)
    pub fn share(&self) -> Self;
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
- [ ] Port `StreamBuffer` from spike to production code
- [ ] Port `StreamHandle` from spike to production code
- [ ] Add `recv_stream_handle()` to PeerConnection
- [ ] Update existing `recv()` to use StreamHandle internally
- [ ] Add unit tests for out-of-order fragment handling
- [ ] Add benchmarks comparing memory usage

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
- [ ] Port `PipedStream` from spike to production code
- [ ] Add `send_fragment()` to PeerConnection
- [ ] Add `pipe_stream()` factory method
- [ ] Add memory pressure management (max buffered fragments)
- [ ] Add metrics for bytes_buffered / bytes_forwarded
- [ ] Add tests for worst-case (reverse order) delivery

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

---

## References

- Spike implementation: `crates/core/src/transport/streaming_spike.rs`
- Design document: `docs/design/stream-based-transport.md`
- Current transport: `crates/core/src/transport/`
- Related PR: Zero-copy optimization #2361

---

## Checklist for Implementation

### Phase 1 Checklist
- [ ] StreamBuffer implementation with fragment tracking
- [ ] StreamHandle with async read interface
- [ ] Notification system for data arrival
- [ ] Unit tests for buffer management
- [ ] Memory usage benchmarks

### Phase 2 Checklist
- [ ] PipedStream implementation
- [ ] Out-of-order fragment buffering
- [ ] Memory pressure management
- [ ] send_fragment() low-level API
- [ ] Integration tests for piped forwarding

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

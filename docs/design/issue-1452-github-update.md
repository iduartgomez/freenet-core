# Leverage Streaming for Optimal Data Transference

## Problem Statement

Large contract transfers (multi-MB) suffer from three bottlenecks:

1. **Full serialization before send** - `bincode::serialize(&data)` requires complete object in memory
2. **Full reassembly at each hop** - `InboundStream` buffers entire message before returning
3. **No piped forwarding** - Intermediate nodes buffer 100% of data before forwarding

**Memory Impact:** 10MB contract traversing 3 hops requires 60MB total (10MB × 2 buffers × 3 nodes) vs ~60KB with piped streaming.

## Spike Validation ✅

Proof-of-concept in `crates/core/src/transport/streaming_spike.rs`:

| Metric | Result |
|--------|--------|
| In-order delivery buffer | **0 bytes** (100% saved) |
| Realistic reordering buffer | **0.2%** of data (12KB for 5MB) |
| Throughput | **1,338 MB/s** |
| First-fragment latency | **106μs** |

---

## Implementation Phases

### Phase 1: StreamHandle Infrastructure

Add incremental access to incoming streams without waiting for completion.

```rust
pub struct StreamHandle {
    pub async fn read_available(&self) -> Bytes;  // Non-blocking
    pub async fn read(&self, max_bytes: usize) -> Bytes;  // Blocking
    pub async fn read_all(&self) -> Bytes;  // Wait for complete
    pub fn is_complete(&self) -> bool;
    pub fn share(&self) -> Self;  // Zero-copy clone
}

pub struct StreamRegistry {
    streams: DashMap<StreamId, StreamHandle>,
    pending_associations: DashMap<StreamId, Transaction>,
}
```

**Key tasks:**
- [ ] Lock-free `StreamBuffer` with `OnceLock<Bytes>` fragment slots
- [ ] Pre-allocate buffer based on `total_size` (avoid realloc)
- [ ] Incremental frontier tracking (O(n) vs O(n²) contiguous calculation)
- [ ] `StreamRegistry` for transport→operations handoff

### Phase 2: Piped Forwarding

Enable intermediate nodes to forward without full reassembly.

```rust
pub struct PipedStream {
    stream_id: StreamId,
    next_to_forward: u32,
    out_of_order: BTreeMap<u32, Bytes>,
    targets: Vec<PeerConnection>,

    pub async fn pipe_fragment(&mut self, frag_num: u32, data: Bytes) -> Result<bool>;
}
```

**Key tasks:**
- [ ] `send_fragment()` low-level API on PeerConnection
- [ ] Bounded backpressure with semaphores per target
- [ ] Memory pressure limits (max buffered fragments)

### Phase 3: Message Layer Integration

Add streaming variants for large transfers.

```rust
pub enum PutMsg {
    RequestPut { contract: ContractContainer, ... },  // Existing

    RequestPutStreaming {  // New
        id: Transaction,
        contract_stream_id: StreamId,
        contract_key: ContractKey,
        contract_size: u64,
        htl: usize,
        target: PeerKeyLocation,
    },
}

pub enum GetMsg {
    Response { result: GetMsgResult, ... },  // Existing

    ResponseStreaming {  // New
        id: Transaction,
        contract_stream_id: StreamId,
        contract_size: u64,
    },
}
```

**Key tasks:**
- [ ] Extend `PutOp` state machine with `PendingStream` tracking
- [ ] Handle race conditions (stream before/after metadata)
- [ ] Two-phase message parsing (header-only for routing decisions)

### Phase 4: Streaming Serialization

Enable incremental serialization for large objects.

```rust
pub trait StreamingSerialize {
    fn start_serialize(&self) -> (u64, StreamingSerializer);
}

impl StreamingSerialize for ContractContainer { ... }
impl StreamingSerialize for WrappedState { ... }
```

### Phase 5: Capability Negotiation

Backward compatibility via handshake.

```rust
struct PeerCapabilities {
    protocol_version: u32,
    streaming_supported: bool,
    piped_forwarding_supported: bool,
}
```

### Phase 6: Gradual Rollout

1. **Shadow mode** - Run both paths, compare results, use traditional
2. **Opt-in** - Users enable via `--streaming-transport` flag
3. **Default on** - Streaming enabled, `--no-streaming-transport` to disable
4. **Cleanup** - Remove legacy code path

---

## Throughput Footguns

Critical performance issues to address during implementation:

| Footgun | Problem | Mitigation |
|---------|---------|------------|
| **Lock contention** | 2M lock/sec on StreamBuffer | Lock-free `OnceLock<Bytes>` slots |
| **Memory allocation** | O(fragments) reallocs | Pre-allocate from `total_size` |
| **Notification overhead** | `Notify` on every fragment | Batch at 64KB thresholds |
| **Contiguous calculation** | O(n²) total | Incremental frontier tracking |
| **LEDBAT waiting** | Busy loop when cwnd full | Async condition variable |
| **Backpressure** | Unbounded queue to slow peers | Bounded semaphores |
| **Encryption CPU** | 50% core on AES-GCM | Batch/parallel encryption |
| **Deserialization** | Full parse before routing | Two-phase header/body |

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
   │                              │   (zero-copy)             │
   │                              │                           │
   │                              ├─► RequestPutStreaming ────┼──►
   │                              │   (new stream_id)         │
   │                              │                           │
   │                              │                           ├─► read_all()
   │                              │                           ├─► deserialize
   │                              │                           ├─► store
   │                              │                           │
   │                              │◄─────────────────────────├─► ResponsePut
   │◄────────────────────────────│                           │
```

**Race condition handling:**
- Stream arrives before metadata → Store in `orphan_streams`, claim later
- Metadata arrives before stream → Register waiter with oneshot channel
- Stream completes before claimed → `was_complete_on_claim()` flag

---

## Future: Zero-Copy Serialization

Consider migrating from bincode to eliminate serialization bottleneck:

| Framework | Benefits | Trade-offs |
|-----------|----------|------------|
| **rkyv** | Zero-copy deserialization, access data directly from buffer | Different archived format, requires derives |
| **flatbuffers** | Already used for topology, schema-based | Requires .fbs files, generated code |

**Recommendation:** Keep bincode for Phase 1-4. Evaluate rkyv for `ContractContainer`/`WrappedState` in future phase—would allow accessing contract metadata before full stream arrives.

---

## Configuration

```rust
pub struct TransportConfig {
    pub streaming_enabled: bool,              // Default: false initially
    pub streaming_threshold_bytes: usize,     // Default: 64KB
    pub piped_forwarding: bool,               // Default: false initially
}
```

```bash
FREENET_STREAMING=true
FREENET_STREAMING_THRESHOLD=65536
FREENET_PIPED_FORWARDING=true
```

---

## Success Metrics

- Memory reduction: **100×** for 10MB contracts at intermediate nodes
- First-byte latency: **< 1ms** (vs full transfer time)
- Zero regressions for small messages (< 64KB)
- Backward compatible with older nodes

---

## Key Files

| File | Purpose |
|------|---------|
| `crates/core/src/transport/streaming_spike.rs` | Proof-of-concept implementation |
| `docs/design/stream-based-transport.md` | Original design investigation |
| `docs/design/issue-1452-streaming-transport-update.md` | Detailed implementation plan |
| `docs/design/streaming-transport-deep-dive.md` | Throughput analysis & ops integration |
| `crates/core/src/transport/peer_connection/` | Current transport layer |

---

## Dependencies

- [x] `Bytes`-based zero-copy fragmentation (PR #2361)
- [x] LEDBAT congestion control
- [x] TokenBucket rate limiting
- [ ] Phase 1-6 implementation

## Related

- PR #2361: Zero-copy stream fragmentation
- Spike branch: `claude/review-stream-transport-AzSg3`

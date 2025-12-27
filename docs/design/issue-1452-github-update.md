# Leverage Streaming for Optimal Data Transference

## Overview

Refactor the transport API to enable streaming of large data transfers (contracts, states) without requiring full buffering at each hop. This significantly reduces memory pressure and latency for large contract operations.

## Problem Statement

**Current bottlenecks:**
1. **Full serialization required** - `PeerConnection::send()` calls `bincode::serialize(&data)` requiring complete object in memory
2. **Full reassembly at each hop** - `InboundStream` accumulates entire message before returning
3. **No piped forwarding** - Intermediate nodes buffer 100% of data before forwarding

**Impact:** For a 10MB contract traversing 3 hops, current approach requires 60MB total memory (10MB × 2 buffers × 3 nodes) vs. ~60KB with piped streaming.

## Spike Validation ✅

A proof-of-concept in `crates/core/src/transport/streaming_spike.rs` validated:

| Metric | Result |
|--------|--------|
| In-order delivery buffer | **0 bytes** (100% saved) |
| Realistic reordering buffer | **0.2%** of data (12KB for 5MB) |
| Throughput | **1,338 MB/s** |
| First-fragment latency | **106μs** |

## Implementation Phases

### Phase 1: StreamHandle Infrastructure
Add incremental access to incoming streams:
```rust
pub struct StreamHandle {
    pub async fn read_available(&self) -> Bytes;
    pub async fn read(&self, max_bytes: usize) -> Bytes;
    pub async fn read_all(&self) -> Bytes;
    pub fn is_complete(&self) -> bool;
}
```

### Phase 2: Piped Forwarding
Enable intermediate nodes to forward without full reassembly:
```rust
pub struct PipedStream {
    pub async fn pipe_fragment(&mut self, frag_num: u32, data: Bytes) -> Result<bool>;
}
```

### Phase 3: Message Layer Integration
Add streaming variants for large transfers:
```rust
pub enum PutMsg {
    RequestPut { ... },                  // Existing (small contracts)
    RequestPutStreaming {                // New (large contracts)
        contract_stream_id: StreamId,
        contract_size: u64,
        ...
    },
}
```

### Phase 4: Streaming Serialization
Enable incremental serialization for ContractContainer:
```rust
pub trait StreamingSerialize {
    fn start_serialize(&self) -> (u64, StreamingSerializer);
}
```

### Phase 5: Capability Negotiation
Backward compatibility via handshake:
```rust
struct PeerCapabilities {
    streaming_supported: bool,
    piped_forwarding_supported: bool,
}
```

### Phase 6: Gradual Rollout
- Shadow mode: Run both paths, compare results
- Opt-in: Users enable via CLI flag
- Default: Streaming enabled by default
- Cleanup: Remove legacy code path

## Future: Zero-Copy Serialization

Consider migrating from bincode to a zero-copy framework (rkyv, flatbuffers) to eliminate the serialization bottleneck entirely:

| Framework | Benefits | Trade-offs |
|-----------|----------|------------|
| **rkyv** | Zero-copy deserialization, access data directly from buffer | Different archived format, requires derives |
| **flatbuffers** | Already used for topology, schema-based | Requires .fbs files, generated code |

**Recommendation:** Keep bincode for Phase 1-4, evaluate rkyv for ContractContainer/WrappedState in a future phase. Zero-copy deserialization would allow accessing contract metadata before the full stream arrives.

## Configuration

```rust
pub struct TransportConfig {
    pub streaming_enabled: bool,              // Default: false initially
    pub streaming_threshold_bytes: usize,     // Default: 64KB
    pub piped_forwarding: bool,               // Default: false initially
}
```

## Key Files

- **Spike**: `crates/core/src/transport/streaming_spike.rs`
- **Design doc**: `docs/design/stream-based-transport.md`
- **Detailed plan**: `docs/design/issue-1452-streaming-transport-update.md`
- **Current transport**: `crates/core/src/transport/peer_connection/`

## Dependencies

- [x] `Bytes`-based zero-copy fragmentation (PR #2361)
- [x] LEDBAT congestion control
- [x] TokenBucket rate limiting
- [ ] Phase 1-6 implementation

## Success Metrics

- Memory reduction: 100× for 10MB contracts at intermediate nodes
- First-byte latency: < 1ms (vs. full transfer time currently)
- Zero regressions for small messages (< 64KB)
- Backward compatible with older nodes

## Related

- PR #2361: Zero-copy stream fragmentation
- Design: `docs/design/stream-based-transport.md`

# Stream-Based Transport Design

## Overview

This document investigates moving Freenet's transport layer from message-based to stream-based, enabling streaming of large data (contracts, states) without collecting them first.

## Current Architecture Analysis

### Data Flow (Current)

```
Application Layer
  ↓ (NetMessage with embedded ContractContainer)
bincode::serialize(&data) → Vec<u8>  ← BOTTLENECK: must collect all data
  ↓
PeerConnection::send()
  ├─→ Small: outbound_short_message() → single packet
  └─→ Large: outbound_stream() → fragment into multiple packets
        ↓
    send_stream(stream_id, data: Vec<u8>, ...)  ← BOTTLENECK: requires complete Vec<u8>
        ↓
    Split into ~1364 byte fragments
        ↓
    Each fragment: encrypt + send via UDP
```

### Key Bottlenecks

1. **Serialization Requires Complete Data**
   - `bincode::serialize(&data)` in `PeerConnection::send<T>()` needs the full object
   - Contracts can be several MBs
   - Must hold entire contract in memory before transmission

2. **Stream Fragmentation is Post-Hoc**
   - `send_stream()` takes `SerializedStream = Vec<u8>`
   - Fragments are created from already-collected data
   - No way to "pipe" data through as it arrives

3. **Forwarding Requires Full Reassembly**
   - Intermediate nodes must reassemble entire stream via `InboundStream`
   - Then re-serialize and re-fragment for next hop
   - Memory pressure grows with contract size

### Current Streaming Implementation

```rust
// outbound_stream.rs - current signature
pub(super) async fn send_stream(
    stream_id: StreamId,
    ...
    stream_to_send: SerializedStream,  // Vec<u8> - complete data required!
    ...
) -> Result<(), TransportError>

// peer_connection.rs - sends require full serialization first
pub async fn send<T>(&mut self, data: T) -> Result<()>
where
    T: Serialize + Send + std::fmt::Debug + 'static,
{
    let data = bincode::serialize(&data).unwrap();  // Full serialization
    if data.len() > MAX_DATA_SIZE {
        self.outbound_stream(data).await;  // Pass complete Vec
    } ...
}
```

## Proposed Architecture

### Core Concept: Connection as Buffer Pointer

Abstract connections so they can be passed as pointers to buffers, enabling incremental consumption of packets from the network interface.

### Stream Handle Abstraction

```rust
/// A handle to a stream being received - can be passed around without copying data
pub struct StreamHandle {
    stream_id: StreamId,
    /// Current read position in the stream
    position: Arc<AtomicU64>,
    /// Total expected length (from first fragment)
    total_length: u64,
    /// Reference to the shared buffer being filled by the network layer
    buffer: Arc<StreamBuffer>,
    /// Notifier for when new data arrives
    data_available: Arc<Notify>,
}

impl StreamHandle {
    /// Read available bytes without waiting (non-blocking)
    pub fn read_available(&self) -> &[u8] { ... }

    /// Async read that waits for more data
    pub async fn read(&self, buf: &mut [u8]) -> io::Result<usize> { ... }

    /// Check how many bytes are available to read
    pub fn available(&self) -> usize { ... }

    /// Check if stream is complete
    pub fn is_complete(&self) -> bool { ... }

    /// Clone handle to share access (zero-copy reference)
    pub fn share(&self) -> StreamHandle { ... }
}

/// Shared buffer that accumulates incoming fragments
struct StreamBuffer {
    /// Ring buffer or growable buffer for incoming data
    data: RwLock<Vec<u8>>,
    /// Bitmap of received fragments for gap detection
    received_fragments: RwLock<BitVec>,
    /// Contiguous bytes available from start
    contiguous_bytes: AtomicU64,
}
```

### Streaming Serialization

Replace single-shot `bincode::serialize` with incremental serialization:

```rust
/// A streaming serializer that yields fragments as they're ready
pub trait StreamingSerialize {
    /// Start serialization, returning expected total size if known
    fn start_serialize(&self) -> (Option<u64>, StreamingSerializer);
}

pub struct StreamingSerializer {
    /// Yields fragments as they become available
    fragment_rx: mpsc::Receiver<Vec<u8>>,
}

impl StreamingSerializer {
    /// Get next fragment (may block waiting for data)
    pub async fn next_fragment(&mut self) -> Option<Vec<u8>> {
        self.fragment_rx.recv().await
    }
}

// Usage for contracts
impl StreamingSerialize for ContractContainer {
    fn start_serialize(&self) -> (Option<u64>, StreamingSerializer) {
        let (tx, rx) = mpsc::channel(16);
        let total_size = self.data().len() as u64 + /* header overhead */;

        // Spawn task that yields fragments
        let data = self.data().clone();
        tokio::spawn(async move {
            // Yield header fragment
            tx.send(serialize_header()).await;

            // Yield data chunks
            for chunk in data.chunks(MAX_FRAGMENT_SIZE) {
                tx.send(chunk.to_vec()).await;
            }
        });

        (Some(total_size), StreamingSerializer { fragment_rx: rx })
    }
}
```

### Piped Streams for Forwarding

Enable forwarding without full reassembly:

```rust
/// A stream that can be forwarded to another peer while being received
pub struct PipedStream {
    /// Incoming fragment receiver
    incoming: StreamHandle,
    /// Outgoing connection(s) to forward to
    outgoing: Vec<PeerConnection>,
    /// Buffer for fragments that arrived out-of-order
    out_of_order_buffer: BTreeMap<u32, Vec<u8>>,
    /// Next fragment number to forward
    next_to_forward: u32,
}

impl PipedStream {
    /// Process incoming fragment and immediately forward if in-order
    pub async fn pipe_fragment(&mut self, fragment_num: u32, data: Vec<u8>) -> Result<()> {
        if fragment_num == self.next_to_forward {
            // In-order: forward immediately
            for conn in &mut self.outgoing {
                conn.send_fragment(self.stream_id, fragment_num, &data).await?;
            }
            self.next_to_forward += 1;

            // Check if we can forward buffered out-of-order fragments
            while let Some(buffered) = self.out_of_order_buffer.remove(&self.next_to_forward) {
                for conn in &mut self.outgoing {
                    conn.send_fragment(self.stream_id, self.next_to_forward, &buffered).await?;
                }
                self.next_to_forward += 1;
            }
        } else {
            // Out-of-order: buffer for later
            self.out_of_order_buffer.insert(fragment_num, data);
        }
        Ok(())
    }
}
```

### New PeerConnection API

```rust
impl PeerConnection {
    /// Send data that implements streaming serialization
    pub async fn send_streaming<T: StreamingSerialize>(&mut self, data: &T) -> Result<StreamId> {
        let stream_id = StreamId::next();
        let (total_size, mut serializer) = data.start_serialize();

        let mut fragment_num = 1u32;
        while let Some(fragment) = serializer.next_fragment().await {
            self.send_fragment(stream_id, total_size, fragment_num, fragment).await?;
            fragment_num += 1;
        }

        Ok(stream_id)
    }

    /// Receive and return a stream handle (doesn't wait for complete data)
    pub async fn recv_stream(&mut self) -> Result<StreamHandle> {
        // Returns immediately when first fragment arrives
        // Caller can read incrementally as more fragments arrive
    }

    /// Start piping a stream to other connections (for forwarding)
    pub fn pipe_stream(&mut self, stream: StreamHandle, targets: Vec<&mut PeerConnection>) -> PipedStream {
        PipedStream {
            incoming: stream,
            outgoing: targets,
            out_of_order_buffer: BTreeMap::new(),
            next_to_forward: 1,
        }
    }

    /// Send a single fragment (low-level API for piping)
    pub async fn send_fragment(
        &mut self,
        stream_id: StreamId,
        total_size: Option<u64>,
        fragment_num: u32,
        data: Vec<u8>,
    ) -> Result<()> {
        // Direct fragment send without collecting
    }
}
```

### Message Layer Changes

Modify network messages to support streaming references:

```rust
#[derive(Serialize, Deserialize)]
pub enum PutMsg {
    /// Original: contains full contract data (keep for small contracts)
    RequestPut {
        contract: ContractContainer,
        value: WrappedState,
        // ...
    },

    /// New: contract data is streamed separately
    RequestPutStreaming {
        id: Transaction,
        /// Stream ID where contract data is being sent
        contract_stream_id: StreamId,
        /// Contract key (can validate once stream completes)
        contract_key: ContractKey,
        /// Size hints for validation
        contract_size: u64,
        value_size: u64,
        htl: usize,
        target: PeerKeyLocation,
    },

    /// Stream data notification (sent on side channel)
    StreamData {
        stream_id: StreamId,
        // Actual data fragments handled at transport layer
    },
}
```

### Integration with Operations

```rust
// In put.rs - handling streaming puts
async fn handle_streaming_put(
    &mut self,
    msg: PutMsg::RequestPutStreaming,
    stream_handle: StreamHandle,
    next_hop: Option<PeerConnection>,
) -> Result<()> {
    match next_hop {
        Some(mut conn) => {
            // Forward to next hop without full reassembly
            let pipe = conn.pipe_stream(stream_handle, vec![]);

            // Start forwarding in background
            tokio::spawn(async move {
                while let Some((frag_num, data)) = pipe.incoming.read_fragment().await {
                    pipe.pipe_fragment(frag_num, data).await?;
                }
            });

            // Send the streaming request message
            conn.send(PutMsg::RequestPutStreaming {
                contract_stream_id: pipe.stream_id(),
                ..msg
            }).await?;
        }
        None => {
            // We're the target - need to reassemble for storage
            let contract_data = stream_handle.read_all().await?;
            let contract = ContractContainer::deserialize(&contract_data)?;
            self.store_contract(contract).await?;
        }
    }
}
```

## Implementation Plan

### Phase 1: Stream Handle Infrastructure
1. Implement `StreamBuffer` with fragment tracking
2. Implement `StreamHandle` with async read interface
3. Add stream notification system (`Notify` based)
4. Unit tests for buffer management

### Phase 2: Transport Layer Changes
1. Modify `InboundStream` to support early access via handles
2. Add `recv_stream()` method to `PeerConnection`
3. Add `send_fragment()` low-level API
4. Keep backwards compatibility with existing `send()/recv()`

### Phase 3: Piped Streams
1. Implement `PipedStream` for forwarding
2. Add out-of-order fragment buffering
3. Handle stream lifecycle (completion, errors)
4. Memory pressure management (limit buffered fragments)

### Phase 4: Message Layer Integration
1. Add `RequestPutStreaming` message variant
2. Implement streaming serialization for `ContractContainer`
3. Modify `PutOp` to handle streaming mode
4. Add configuration for stream vs message mode threshold

### Phase 5: Full Pipeline Integration
1. End-to-end streaming puts
2. Streaming gets (contract retrieval)
3. Performance benchmarks
4. Memory usage validation

## Key Design Decisions

### 1. When to Use Streaming vs Message Mode

```rust
const STREAMING_THRESHOLD: usize = 64 * 1024; // 64KB

impl PeerConnection {
    pub async fn send_auto<T>(&mut self, data: &T) -> Result<()>
    where T: Serialize + StreamingSerialize
    {
        let size_estimate = data.size_hint();
        if size_estimate > STREAMING_THRESHOLD {
            self.send_streaming(data).await?;
        } else {
            self.send(data).await?;
        }
        Ok(())
    }
}
```

### 2. Memory Pressure Management

```rust
struct StreamConfig {
    /// Max fragments to buffer for out-of-order arrival
    max_buffered_fragments: usize,
    /// Max total bytes in flight per stream
    max_stream_buffer_bytes: usize,
    /// Max concurrent streams per connection
    max_concurrent_streams: usize,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            max_buffered_fragments: 1000,      // ~1.3MB at 1364 bytes/fragment
            max_stream_buffer_bytes: 10 << 20, // 10MB
            max_concurrent_streams: 10,
        }
    }
}
```

### 3. Error Handling

```rust
enum StreamError {
    /// Stream timed out waiting for fragments
    Timeout,
    /// Too many fragments buffered (memory pressure)
    BufferOverflow,
    /// Fragment out of expected range
    InvalidFragment,
    /// Connection closed mid-stream
    ConnectionLost,
    /// Verification failed (hash mismatch)
    VerificationFailed,
}
```

### 4. Stream Verification

```rust
impl StreamHandle {
    /// Verify stream integrity once complete
    pub async fn verify(&self, expected_hash: &[u8; 32]) -> Result<bool> {
        if !self.is_complete() {
            return Err(StreamError::Incomplete);
        }

        let data = self.buffer.data.read();
        let actual_hash = blake3::hash(&data);
        Ok(actual_hash.as_bytes() == expected_hash)
    }
}
```

## Comparison: Current vs Proposed

| Aspect | Current | Proposed |
|--------|---------|----------|
| Memory for 10MB contract | 10MB (full buffer) | ~1.3MB (sliding window) |
| Forwarding latency | Wait for complete + re-serialize | First fragment delay only |
| Intermediate node load | Full reassembly required | Pass-through possible |
| API complexity | Simple `send(T)` | `send_streaming()` + handles |
| Backwards compatibility | N/A | Maintained via auto-detection |

## Open Questions

1. **Fragment ordering guarantees**: Should we require strict ordering for piped streams, or allow reordering with larger buffers?

2. **Partial stream cancellation**: How to handle a stream that's abandoned mid-transfer?

3. **Multiplexing**: Should multiple streams share fragment-level interleaving, or use separate logical channels?

4. **Encryption boundaries**: Current encryption is per-packet. Does streaming change nonce management?

5. **Congestion control**: How does streaming interact with the existing rate limiter?

## References

- Current transport implementation: `crates/core/src/transport/`
- Existing stream handling: `peer_connection/outbound_stream.rs`, `inbound_stream.rs`
- Contract structures: `freenet_stdlib::prelude::ContractContainer`

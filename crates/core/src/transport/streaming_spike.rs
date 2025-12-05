//! # Streaming Transport Spike
//!
//! Proof-of-concept for stream-based transport with zero-copy forwarding.
//! This is isolated experimental code - not integrated with production transport.
//!
//! ## Goals
//! 1. Validate `Bytes`-based zero-copy buffer sharing
//! 2. Demonstrate piped stream forwarding without full reassembly
//! 3. Measure memory overhead vs current approach
//!
//! ## Non-goals (for spike)
//! - Integration with existing PeerConnection
//! - Encryption/reliability
//! - Full error handling

use bytes::{Bytes, BytesMut};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Notify, RwLock};

/// Fragment size matching current transport
const FRAGMENT_SIZE: usize = 1364;

/// Unique stream identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StreamId(u32);

impl StreamId {
    pub fn next() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

// ============================================================================
// StreamBuffer - shared buffer accumulating incoming fragments
// ============================================================================

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
    /// Fragment numbers are 1-indexed
    received_fragments: RwLock<Vec<bool>>,
    /// Notify waiters when new data arrives
    data_available: Notify,
}

impl StreamBuffer {
    pub fn new(total_size: u64) -> Self {
        let num_fragments = (total_size as usize).div_ceil(FRAGMENT_SIZE);
        Self {
            data: RwLock::new(BytesMut::with_capacity(total_size as usize)),
            total_size: AtomicU64::new(total_size),
            contiguous_bytes: AtomicU64::new(0),
            received_fragments: RwLock::new(vec![false; num_fragments]),
            data_available: Notify::new(),
        }
    }

    /// Insert a fragment into the buffer
    pub async fn insert_fragment(&self, fragment_num: u32, data: &[u8]) {
        let idx = (fragment_num - 1) as usize; // 1-indexed
        let offset = idx * FRAGMENT_SIZE;

        {
            let mut buf = self.data.write().await;
            // Ensure buffer is large enough
            if buf.len() < offset + data.len() {
                buf.resize(offset + data.len(), 0);
            }
            // Copy fragment data into position
            buf[offset..offset + data.len()].copy_from_slice(data);
        }

        // Mark fragment as received
        {
            let mut received = self.received_fragments.write().await;
            if idx < received.len() {
                received[idx] = true;
            }

            // Update contiguous count
            let mut contiguous = 0u64;
            for (i, &recv) in received.iter().enumerate() {
                if recv {
                    let frag_size = if i == received.len() - 1 {
                        // Last fragment might be smaller
                        let total = self.total_size.load(Ordering::Acquire) as usize;
                        total - (i * FRAGMENT_SIZE)
                    } else {
                        FRAGMENT_SIZE
                    };
                    contiguous += frag_size as u64;
                } else {
                    break;
                }
            }
            self.contiguous_bytes.store(contiguous, Ordering::Release);
        }

        self.data_available.notify_waiters();
    }

    /// Get contiguous data as zero-copy Bytes slice
    pub async fn get_contiguous(&self) -> Bytes {
        let contiguous = self.contiguous_bytes.load(Ordering::Acquire) as usize;
        if contiguous == 0 {
            return Bytes::new();
        }
        let data = self.data.read().await;
        // Clone BytesMut -> freeze -> slice is all zero-copy for the data
        data.clone().freeze().split_to(contiguous)
    }

    /// Check if stream is complete
    pub fn is_complete(&self) -> bool {
        let contiguous = self.contiguous_bytes.load(Ordering::Acquire);
        let total = self.total_size.load(Ordering::Acquire);
        contiguous >= total
    }

    /// Wait for more data
    pub async fn wait_for_data(&self) {
        self.data_available.notified().await;
    }
}

// ============================================================================
// StreamHandle - shareable reference to a stream being received
// ============================================================================

/// A handle to a stream being received. Can be cloned and shared
/// without copying the underlying data (uses Arc + Bytes).
#[derive(Clone)]
pub struct StreamHandle {
    pub stream_id: StreamId,
    buffer: Arc<StreamBuffer>,
    /// Local read position for this handle
    read_position: Arc<AtomicU64>,
}

impl StreamHandle {
    pub fn new(stream_id: StreamId, total_size: u64) -> Self {
        Self {
            stream_id,
            buffer: Arc::new(StreamBuffer::new(total_size)),
            read_position: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Insert a fragment (called by network layer)
    pub async fn push_fragment(&self, fragment_num: u32, data: &[u8]) {
        self.buffer.insert_fragment(fragment_num, data).await;
    }

    /// Read available data without blocking (returns what's ready)
    pub async fn read_available(&self) -> Bytes {
        let pos = self.read_position.load(Ordering::Acquire) as usize;
        let data = self.buffer.get_contiguous().await;
        if data.len() > pos {
            data.slice(pos..)
        } else {
            Bytes::new()
        }
    }

    /// Blocking read - waits for data if none available
    pub async fn read(&self, max_bytes: usize) -> Bytes {
        loop {
            let available = self.read_available().await;
            if !available.is_empty() {
                let take = available.len().min(max_bytes);
                let result = available.slice(..take);
                self.read_position.fetch_add(take as u64, Ordering::Release);
                return result;
            }
            if self.is_complete() {
                return Bytes::new(); // EOF
            }
            self.buffer.wait_for_data().await;
        }
    }

    /// Read all data (waits for completion)
    pub async fn read_all(&self) -> Bytes {
        while !self.is_complete() {
            self.buffer.wait_for_data().await;
        }
        self.buffer.get_contiguous().await
    }

    pub fn is_complete(&self) -> bool {
        self.buffer.is_complete()
    }

    /// Get a shared handle (cheap - just Arc clone)
    pub fn share(&self) -> Self {
        Self {
            stream_id: self.stream_id,
            buffer: Arc::clone(&self.buffer),
            read_position: Arc::new(AtomicU64::new(0)), // Fresh read position
        }
    }

    /// Get contiguous bytes available
    pub fn available_bytes(&self) -> u64 {
        self.buffer.contiguous_bytes.load(Ordering::Acquire)
    }
}

// ============================================================================
// PipedStream - forward fragments without full reassembly
// ============================================================================

/// Simulated outbound connection for the spike
pub struct MockOutbound {
    pub sent_fragments: Arc<RwLock<Vec<(StreamId, u32, Bytes)>>>,
    pub sender: mpsc::Sender<(StreamId, u32, Bytes)>,
}

impl MockOutbound {
    pub fn new() -> (Self, mpsc::Receiver<(StreamId, u32, Bytes)>) {
        let (tx, rx) = mpsc::channel(100);
        (
            Self {
                sent_fragments: Arc::new(RwLock::new(Vec::new())),
                sender: tx,
            },
            rx,
        )
    }

    /// Create a mock outbound that discards sent data (for benchmarks)
    pub fn new_discard() -> Self {
        let (tx, _) = mpsc::channel(1); // Receiver dropped, sends will return error
        Self {
            sent_fragments: Arc::new(RwLock::new(Vec::new())),
            sender: tx,
        }
    }

    pub async fn send_fragment(&self, stream_id: StreamId, frag_num: u32, data: Bytes) {
        self.sent_fragments.write().await.push((stream_id, frag_num, data.clone()));
        let _ = self.sender.send((stream_id, frag_num, data)).await;
    }
}

/// Forwards stream fragments to another connection without full reassembly.
/// Key insight: we can forward in-order fragments immediately, buffer out-of-order ones.
pub struct PipedStream {
    stream_id: StreamId,
    total_size: u64,
    /// Next fragment number we're waiting to forward
    next_to_forward: u32,
    /// Out-of-order fragments buffered for later
    /// Using Bytes means no data copying when we buffer
    out_of_order: BTreeMap<u32, Bytes>,
    /// Target connection(s)
    targets: Vec<MockOutbound>,
    /// Track bytes forwarded (for metrics)
    bytes_forwarded: u64,
    /// Track bytes buffered (for memory pressure monitoring)
    bytes_buffered: u64,
}

impl PipedStream {
    pub fn new(stream_id: StreamId, total_size: u64, targets: Vec<MockOutbound>) -> Self {
        Self {
            stream_id,
            total_size,
            next_to_forward: 1, // 1-indexed
            out_of_order: BTreeMap::new(),
            targets,
            bytes_forwarded: 0,
            bytes_buffered: 0,
        }
    }

    /// Process an incoming fragment - forward immediately if in-order,
    /// buffer if out-of-order. Returns true if stream is complete.
    pub async fn pipe_fragment(&mut self, frag_num: u32, data: Bytes) -> bool {
        if frag_num == self.next_to_forward {
            // In-order: forward immediately to all targets
            self.forward_to_all(frag_num, data.clone()).await;
            self.bytes_forwarded += data.len() as u64;
            self.next_to_forward += 1;

            // Check if we can now forward buffered fragments
            while let Some(buffered) = self.out_of_order.remove(&self.next_to_forward) {
                let len = buffered.len() as u64;
                self.forward_to_all(self.next_to_forward, buffered).await;
                self.bytes_forwarded += len;
                self.bytes_buffered -= len;
                self.next_to_forward += 1;
            }
        } else if frag_num > self.next_to_forward {
            // Out-of-order: buffer for later
            // Note: Bytes clone is cheap (ref-count bump)
            self.bytes_buffered += data.len() as u64;
            self.out_of_order.insert(frag_num, data);
        }
        // else: duplicate or old fragment, ignore

        self.bytes_forwarded >= self.total_size
    }

    async fn forward_to_all(&self, frag_num: u32, data: Bytes) {
        for target in &self.targets {
            // Bytes::clone is cheap - just ref-count bump
            target.send_fragment(self.stream_id, frag_num, data.clone()).await;
        }
    }

    pub fn bytes_buffered(&self) -> u64 {
        self.bytes_buffered
    }

    #[allow(dead_code)] // Useful for metrics, will be used in integration
    pub fn bytes_forwarded(&self) -> u64 {
        self.bytes_forwarded
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Test basic StreamHandle functionality
    #[tokio::test]
    async fn test_stream_handle_basic() {
        let total_size = 3000u64; // About 3 fragments
        let handle = StreamHandle::new(StreamId::next(), total_size);

        // Simulate receiving fragments
        let data1 = vec![1u8; FRAGMENT_SIZE];
        let data2 = vec![2u8; FRAGMENT_SIZE];
        let data3 = vec![3u8; total_size as usize - 2 * FRAGMENT_SIZE];

        handle.push_fragment(1, &data1).await;
        assert!(!handle.is_complete());
        assert_eq!(handle.available_bytes(), FRAGMENT_SIZE as u64);

        handle.push_fragment(2, &data2).await;
        assert!(!handle.is_complete());

        handle.push_fragment(3, &data3).await;
        assert!(handle.is_complete());

        // Read all data
        let all_data = handle.read_all().await;
        assert_eq!(all_data.len(), total_size as usize);
        assert_eq!(&all_data[..FRAGMENT_SIZE], &data1[..]);
        assert_eq!(&all_data[FRAGMENT_SIZE..2*FRAGMENT_SIZE], &data2[..]);
    }

    /// Test out-of-order fragment delivery
    #[tokio::test]
    async fn test_stream_handle_out_of_order() {
        let total_size = 3000u64;
        let handle = StreamHandle::new(StreamId::next(), total_size);

        let data1 = vec![1u8; FRAGMENT_SIZE];
        let data2 = vec![2u8; FRAGMENT_SIZE];
        let data3 = vec![3u8; total_size as usize - 2 * FRAGMENT_SIZE];

        // Receive out of order: 3, 1, 2
        handle.push_fragment(3, &data3).await;
        assert_eq!(handle.available_bytes(), 0); // Can't read yet

        handle.push_fragment(1, &data1).await;
        assert_eq!(handle.available_bytes(), FRAGMENT_SIZE as u64);

        handle.push_fragment(2, &data2).await;
        assert!(handle.is_complete());
    }

    /// Test zero-copy sharing
    #[tokio::test]
    async fn test_stream_handle_sharing() {
        let total_size = 2000u64;
        let handle1 = StreamHandle::new(StreamId::next(), total_size);

        handle1.push_fragment(1, &vec![42u8; FRAGMENT_SIZE]).await;

        // Share the handle
        let handle2 = handle1.share();

        // Both can read the same data
        let data1 = handle1.read_available().await;
        let data2 = handle2.read_available().await;

        assert_eq!(data1.len(), data2.len());
        assert_eq!(&data1[..], &data2[..]);

        // Verify it's the same underlying data (Bytes uses Arc internally)
        // Reading from one doesn't affect the other's position
        let read1 = handle1.read(100).await;
        let read2 = handle2.read(100).await;
        assert_eq!(&read1[..], &read2[..]);
    }

    /// Test piped stream forwarding
    #[tokio::test]
    async fn test_piped_stream_in_order() {
        let stream_id = StreamId::next();
        let total_size = 3000u64;

        let (target, mut rx) = MockOutbound::new();
        let mut pipe = PipedStream::new(stream_id, total_size, vec![target]);

        // Send fragments in order
        let frag1 = Bytes::from(vec![1u8; FRAGMENT_SIZE]);
        let frag2 = Bytes::from(vec![2u8; FRAGMENT_SIZE]);
        let frag3 = Bytes::from(vec![3u8; total_size as usize - 2 * FRAGMENT_SIZE]);

        assert!(!pipe.pipe_fragment(1, frag1.clone()).await);
        assert!(!pipe.pipe_fragment(2, frag2.clone()).await);
        assert!(pipe.pipe_fragment(3, frag3.clone()).await); // Complete

        // Verify all fragments were forwarded
        let mut received = Vec::new();
        while let Ok(item) = rx.try_recv() {
            received.push(item);
        }
        assert_eq!(received.len(), 3);
        assert_eq!(received[0].1, 1);
        assert_eq!(received[1].1, 2);
        assert_eq!(received[2].1, 3);

        // No buffering should have occurred
        assert_eq!(pipe.bytes_buffered(), 0);
    }

    /// Test piped stream with out-of-order fragments
    #[tokio::test]
    async fn test_piped_stream_out_of_order() {
        let stream_id = StreamId::next();
        let total_size = 3000u64;

        let (target, mut rx) = MockOutbound::new();
        let mut pipe = PipedStream::new(stream_id, total_size, vec![target]);

        let frag1 = Bytes::from(vec![1u8; FRAGMENT_SIZE]);
        let frag2 = Bytes::from(vec![2u8; FRAGMENT_SIZE]);
        let frag3 = Bytes::from(vec![3u8; total_size as usize - 2 * FRAGMENT_SIZE]);

        // Send out of order: 3, 2, 1
        assert!(!pipe.pipe_fragment(3, frag3.clone()).await);
        assert_eq!(pipe.bytes_buffered(), frag3.len() as u64);

        assert!(!pipe.pipe_fragment(2, frag2.clone()).await);
        assert_eq!(pipe.bytes_buffered(), (frag2.len() + frag3.len()) as u64);

        // Fragment 1 triggers forwarding of all
        assert!(pipe.pipe_fragment(1, frag1.clone()).await);
        assert_eq!(pipe.bytes_buffered(), 0);

        // Verify correct order in output
        let mut received = Vec::new();
        while let Ok(item) = rx.try_recv() {
            received.push(item);
        }
        assert_eq!(received.len(), 3);
        assert_eq!(received[0].1, 1);
        assert_eq!(received[1].1, 2);
        assert_eq!(received[2].1, 3);
    }

    /// Benchmark: Compare memory for forwarding
    /// Note: Using 1MB in debug mode for fast iteration
    #[tokio::test]
    async fn test_memory_comparison() {
        const DATA_SIZE: usize = 1 * 1024 * 1024; // 1MB
        let num_fragments = DATA_SIZE.div_ceil(FRAGMENT_SIZE);

        // Current approach: would need full buffer
        let current_approach_memory = DATA_SIZE;

        // Piped approach: worst case is all out-of-order
        // Best case (in-order): 0 buffering
        let stream_id = StreamId::next();
        let target = MockOutbound::new_discard();
        let mut pipe = PipedStream::new(stream_id, DATA_SIZE as u64, vec![target]);

        // Simulate in-order delivery
        for i in 1..=num_fragments {
            let size = if i == num_fragments {
                DATA_SIZE - (num_fragments - 1) * FRAGMENT_SIZE
            } else {
                FRAGMENT_SIZE
            };
            let frag = Bytes::from(vec![0u8; size]);
            pipe.pipe_fragment(i as u32, frag).await;
        }

        let piped_max_buffer = pipe.bytes_buffered();
        println!("Current approach memory: {} bytes", current_approach_memory);
        println!("Piped approach max buffer (in-order): {} bytes", piped_max_buffer);
        println!("Memory saved: {} bytes", current_approach_memory as i64 - piped_max_buffer as i64);

        assert_eq!(piped_max_buffer, 0, "In-order should need no buffering");
    }

    /// Test worst case: all fragments arrive in reverse order
    #[tokio::test]
    async fn test_piped_stream_worst_case() {
        const DATA_SIZE: usize = 100 * 1024; // 100KB for test speed
        let num_fragments = DATA_SIZE.div_ceil(FRAGMENT_SIZE);

        let stream_id = StreamId::next();
        let target = MockOutbound::new_discard();
        let mut pipe = PipedStream::new(stream_id, DATA_SIZE as u64, vec![target]);

        let mut max_buffered = 0u64;

        // Send in reverse order (worst case)
        for i in (1..=num_fragments).rev() {
            let size = if i == num_fragments {
                DATA_SIZE - (num_fragments - 1) * FRAGMENT_SIZE
            } else {
                FRAGMENT_SIZE
            };
            let frag = Bytes::from(vec![0u8; size]);
            pipe.pipe_fragment(i as u32, frag).await;
            max_buffered = max_buffered.max(pipe.bytes_buffered());
        }

        println!("Worst case (reverse order) max buffer: {} bytes", max_buffered);
        println!("Total data size: {} bytes", DATA_SIZE);

        // In worst case, we buffer almost everything until fragment 1 arrives
        assert!(max_buffered > 0);
        // After fragment 1 arrives, everything should drain
        assert_eq!(pipe.bytes_buffered(), 0);
    }

    // ========================================================================
    // Performance Tests
    // ========================================================================

    /// Performance test: Measure throughput for in-order fragment processing
    #[tokio::test]
    async fn perf_test_in_order_throughput() {
        const DATA_SIZE: usize = 10 * 1024 * 1024; // 10MB
        let num_fragments = DATA_SIZE.div_ceil(FRAGMENT_SIZE);

        let stream_id = StreamId::next();
        let target = MockOutbound::new_discard();
        let mut pipe = PipedStream::new(stream_id, DATA_SIZE as u64, vec![target]);

        // Pre-generate fragment data
        let fragment_data: Vec<Bytes> = (1..=num_fragments)
            .map(|i| {
                let size = if i == num_fragments {
                    DATA_SIZE - (num_fragments - 1) * FRAGMENT_SIZE
                } else {
                    FRAGMENT_SIZE
                };
                Bytes::from(vec![0u8; size])
            })
            .collect();

        let start = Instant::now();

        for (i, frag) in fragment_data.into_iter().enumerate() {
            pipe.pipe_fragment((i + 1) as u32, frag).await;
        }

        let elapsed = start.elapsed();
        let throughput_mbps = (DATA_SIZE as f64 / 1_000_000.0) / elapsed.as_secs_f64();

        println!("=== In-Order Throughput Test ===");
        println!("Data size: {} MB", DATA_SIZE / 1_000_000);
        println!("Fragments: {}", num_fragments);
        println!("Time: {:?}", elapsed);
        println!("Throughput: {:.2} MB/s", throughput_mbps);

        // Sanity check: should complete reasonably fast
        assert!(elapsed.as_secs() < 30, "In-order processing too slow");
    }

    /// Performance test: Measure throughput for StreamHandle with concurrent readers
    #[tokio::test]
    async fn perf_test_concurrent_readers() {
        const DATA_SIZE: usize = 1 * 1024 * 1024; // 1MB
        let num_fragments = DATA_SIZE.div_ceil(FRAGMENT_SIZE);

        let handle = StreamHandle::new(StreamId::next(), DATA_SIZE as u64);

        // Create multiple shared handles
        let handles: Vec<_> = (0..4).map(|_| handle.share()).collect();

        // Pre-generate fragment data
        let fragment_data: Vec<Vec<u8>> = (1..=num_fragments)
            .map(|i| {
                let size = if i == num_fragments {
                    DATA_SIZE - (num_fragments - 1) * FRAGMENT_SIZE
                } else {
                    FRAGMENT_SIZE
                };
                vec![(i % 256) as u8; size]
            })
            .collect();

        let start = Instant::now();

        // Push all fragments
        for (i, frag) in fragment_data.iter().enumerate() {
            handle.push_fragment((i + 1) as u32, frag).await;
        }

        // All readers should be able to read the complete data
        let mut read_times = Vec::new();
        for h in handles {
            let reader_start = Instant::now();
            let data = h.read_all().await;
            read_times.push(reader_start.elapsed());
            assert_eq!(data.len(), DATA_SIZE);
        }

        let elapsed = start.elapsed();

        println!("=== Concurrent Readers Test ===");
        println!("Data size: {} MB", DATA_SIZE / 1_000_000);
        println!("Readers: 4");
        println!("Total time: {:?}", elapsed);
        println!("Read times: {:?}", read_times);

        // All readers should complete almost instantly after data is available
        for (i, t) in read_times.iter().enumerate() {
            assert!(t.as_millis() < 100, "Reader {} too slow: {:?}", i, t);
        }
    }

    /// Performance test: Measure latency from first fragment to first read
    #[tokio::test]
    async fn perf_test_first_fragment_latency() {
        const DATA_SIZE: usize = 100 * 1024; // 100KB
        let handle = StreamHandle::new(StreamId::next(), DATA_SIZE as u64);

        let fragment_data = vec![42u8; FRAGMENT_SIZE];

        let start = Instant::now();

        // Push first fragment
        handle.push_fragment(1, &fragment_data).await;

        // Measure time to read available data
        let data = handle.read_available().await;
        let latency = start.elapsed();

        println!("=== First Fragment Latency Test ===");
        println!("Latency to first read: {:?}", latency);
        println!("Bytes available: {}", data.len());

        assert_eq!(data.len(), FRAGMENT_SIZE);
        assert!(latency.as_micros() < 1000, "First fragment latency too high: {:?}", latency);
    }

    /// Performance test: Compare piped vs full-reassembly memory patterns
    #[tokio::test]
    async fn perf_test_memory_patterns() {
        const DATA_SIZE: usize = 5 * 1024 * 1024; // 5MB
        let num_fragments = DATA_SIZE.div_ceil(FRAGMENT_SIZE);

        println!("=== Memory Patterns Test ===");
        println!("Data size: {} MB", DATA_SIZE / 1_000_000);
        println!("Fragment size: {} bytes", FRAGMENT_SIZE);
        println!("Number of fragments: {}", num_fragments);

        // Test 1: In-order delivery (best case for piped)
        {
            let stream_id = StreamId::next();
            let target = MockOutbound::new_discard();
            let mut pipe = PipedStream::new(stream_id, DATA_SIZE as u64, vec![target]);
            let mut max_buffer = 0u64;

            for i in 1..=num_fragments {
                let size = if i == num_fragments {
                    DATA_SIZE - (num_fragments - 1) * FRAGMENT_SIZE
                } else {
                    FRAGMENT_SIZE
                };
                pipe.pipe_fragment(i as u32, Bytes::from(vec![0u8; size])).await;
                max_buffer = max_buffer.max(pipe.bytes_buffered());
            }

            println!("\nIn-order delivery:");
            println!("  Max buffer: {} bytes ({}% of data)", max_buffer, max_buffer * 100 / DATA_SIZE as u64);
            assert_eq!(max_buffer, 0);
        }

        // Test 2: Random-ish delivery (realistic case)
        {
            let stream_id = StreamId::next();
            let target = MockOutbound::new_discard();
            let mut pipe = PipedStream::new(stream_id, DATA_SIZE as u64, vec![target]);
            let mut max_buffer = 0u64;

            // Simulate network reordering: send in chunks with gaps
            let chunk_size = 10;
            let mut order: Vec<usize> = (1..=num_fragments).collect();
            // Shuffle within chunks of 10
            for chunk in order.chunks_mut(chunk_size) {
                chunk.reverse();
            }

            for i in order {
                let size = if i == num_fragments {
                    DATA_SIZE - (num_fragments - 1) * FRAGMENT_SIZE
                } else {
                    FRAGMENT_SIZE
                };
                pipe.pipe_fragment(i as u32, Bytes::from(vec![0u8; size])).await;
                max_buffer = max_buffer.max(pipe.bytes_buffered());
            }

            println!("\nChunked-reverse delivery (realistic reordering):");
            println!("  Max buffer: {} bytes ({}% of data)", max_buffer, max_buffer * 100 / DATA_SIZE as u64);
            // With chunk_size=10, worst case per chunk is ~10 fragments buffered
            assert!(max_buffer < (chunk_size * FRAGMENT_SIZE) as u64 * 2);
        }

        // Test 3: Full reassembly baseline (current approach)
        {
            let handle = StreamHandle::new(StreamId::next(), DATA_SIZE as u64);

            for i in 1..=num_fragments {
                let size = if i == num_fragments {
                    DATA_SIZE - (num_fragments - 1) * FRAGMENT_SIZE
                } else {
                    FRAGMENT_SIZE
                };
                handle.push_fragment(i as u32, &vec![0u8; size]).await;
            }

            println!("\nFull reassembly (current approach baseline):");
            println!("  Buffer size: {} bytes (100% of data)", DATA_SIZE);
        }
    }
}

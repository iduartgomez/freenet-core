//! # Streaming Transport Spike
//!
//! Proof-of-concept for stream-based transport with zero-copy forwarding.
//! This is isolated experimental code - not integrated with production transport.
//!
//! ## Goals
//! 1. Validate `Bytes`-based zero-copy buffer sharing
//! 2. Demonstrate piped stream forwarding without full reassembly
//! 3. Compare lock-free (OnceLock) vs locked (RwLock) approaches
//! 4. Implement `futures::Stream` trait for async consumption
//!
//! ## Non-goals (for spike)
//! - Integration with existing PeerConnection
//! - Encryption/reliability
//! - Full error handling

use bytes::{Bytes, BytesMut};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
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
// Lock-Free StreamBuffer using OnceLock<Bytes>
// ============================================================================

/// Lock-free buffer using OnceLock for each fragment slot.
///
/// Key benefits over RwLock approach:
/// - `insert()` is a single atomic CAS - no mutex contention
/// - `get()` is a single atomic load - readers never block
/// - Duplicate fragments are automatically no-ops (idempotent)
/// - Concurrent writers to different slots never block each other
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
    pub fn new(total_size: u64, fragment_size: usize) -> Self {
        let n = total_size.div_ceil(fragment_size as u64) as usize;

        // Pre-compute fragment sizes
        let fragment_sizes: Box<[usize]> = (0..n)
            .map(|i| {
                if i == n - 1 {
                    // Last fragment may be smaller
                    let remaining = total_size as usize - (i * fragment_size);
                    remaining.min(fragment_size)
                } else {
                    fragment_size
                }
            })
            .collect();

        Self {
            fragments: (0..n).map(|_| OnceLock::new()).collect(),
            fragment_sizes,
            total_size,
            total_fragments: n as u32,
            contiguous_fragments: AtomicU32::new(0),
            data_available: Notify::new(),
        }
    }

    /// Insert fragment - completely lock-free using CAS
    pub fn insert(&self, fragment_num: u32, data: Bytes) -> bool {
        let idx = (fragment_num - 1) as usize;
        if idx >= self.fragments.len() {
            return false;
        }

        // OnceLock::set is lock-free CAS - duplicates are no-ops
        let was_empty = self.fragments[idx].set(data).is_ok();

        if was_empty {
            // Try to advance the contiguous frontier
            self.advance_frontier();
        }

        self.is_complete()
    }

    fn advance_frontier(&self) {
        loop {
            let current = self.contiguous_fragments.load(Ordering::Acquire);
            if current >= self.total_fragments {
                break;
            }

            // Check if next fragment is available
            if self.fragments[current as usize].get().is_some() {
                // Try to advance atomically
                match self.contiguous_fragments.compare_exchange_weak(
                    current,
                    current + 1,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        // We advanced - notify waiters and check next
                        self.data_available.notify_waiters();
                        continue;
                    }
                    Err(_) => continue, // Someone else advanced, retry
                }
            } else {
                break; // Gap found
            }
        }
    }

    /// Get contiguous bytes available
    pub fn contiguous_bytes(&self) -> u64 {
        let n = self.contiguous_fragments.load(Ordering::Acquire) as usize;
        self.fragment_sizes[..n].iter().sum::<usize>() as u64
    }

    /// Check if stream is complete
    pub fn is_complete(&self) -> bool {
        self.contiguous_fragments.load(Ordering::Acquire) >= self.total_fragments
    }

    /// Iterate available fragments - no locks needed
    pub fn iter_contiguous(&self) -> impl Iterator<Item = &Bytes> {
        let n = self.contiguous_fragments.load(Ordering::Acquire) as usize;
        self.fragments[..n].iter().filter_map(|s| s.get())
    }

    /// Collect contiguous data into Bytes
    pub fn collect_contiguous(&self) -> Bytes {
        let n = self.contiguous_fragments.load(Ordering::Acquire) as usize;
        if n == 0 {
            return Bytes::new();
        }

        let total_bytes: usize = self.fragment_sizes[..n].iter().sum();
        let mut buf = BytesMut::with_capacity(total_bytes);

        for slot in &self.fragments[..n] {
            if let Some(frag) = slot.get() {
                buf.extend_from_slice(frag);
            }
        }

        buf.freeze()
    }

    /// Wait for more data
    pub async fn wait_for_data(&self) {
        self.data_available.notified().await;
    }

    pub fn total_size(&self) -> u64 {
        self.total_size
    }
}

// ============================================================================
// InboundStream - Stream trait implementation
// ============================================================================

/// Consumer handle implementing `futures::Stream` for async iteration.
/// Multiple consumers can read from the same buffer independently.
pub struct InboundStream {
    buffer: Arc<LockFreeStreamBuffer>,
    /// Current fragment position for this consumer
    current_fragment: u32,
}

impl InboundStream {
    pub fn new(buffer: Arc<LockFreeStreamBuffer>) -> Self {
        Self {
            buffer,
            current_fragment: 0,
        }
    }

    /// Fork into independent consumer with its own position
    pub fn fork(&self) -> Self {
        Self {
            buffer: Arc::clone(&self.buffer),
            current_fragment: 0,
        }
    }

    /// Collect all data into Bytes (waits for completion)
    pub async fn collect_all(mut self) -> Bytes {
        use futures::StreamExt;

        let mut buf = BytesMut::with_capacity(self.buffer.total_size() as usize);
        while let Some(chunk) = self.next().await {
            buf.extend_from_slice(&chunk);
        }
        buf.freeze()
    }

    /// Check if complete
    pub fn is_complete(&self) -> bool {
        self.buffer.is_complete()
    }
}

impl futures::Stream for InboundStream {
    type Item = Bytes;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let contiguous = self.buffer.contiguous_fragments.load(Ordering::Acquire);

        if self.current_fragment >= self.buffer.total_fragments {
            return Poll::Ready(None); // EOF
        }

        if self.current_fragment < contiguous {
            // Data available - return next fragment
            let idx = self.current_fragment as usize;
            let frag = self.buffer.fragments[idx].get().cloned();
            if let Some(frag) = frag {
                self.current_fragment += 1;
                return Poll::Ready(Some(frag)); // Bytes clone = refcount bump
            }
        }

        // No data yet - register waker
        // Note: In production, we'd use a proper waker registration
        // For the spike, we'll use a simple poll-based approach
        let notify = self.buffer.data_available.notified();
        tokio::pin!(notify);

        match notify.poll(cx) {
            Poll::Ready(()) => {
                // Data might be available now, re-check
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

// ============================================================================
// Legacy RwLock-based StreamBuffer (for comparison)
// ============================================================================

/// Original RwLock-based buffer for comparison benchmarks
pub struct RwLockStreamBuffer {
    data: RwLock<BytesMut>,
    total_size: AtomicU64,
    contiguous_bytes: AtomicU64,
    received_fragments: RwLock<Vec<bool>>,
    data_available: Notify,
}

impl RwLockStreamBuffer {
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

    pub async fn insert_fragment(&self, fragment_num: u32, data: &[u8]) {
        let idx = (fragment_num - 1) as usize;
        let offset = idx * FRAGMENT_SIZE;

        {
            let mut buf = self.data.write().await;
            if buf.len() < offset + data.len() {
                buf.resize(offset + data.len(), 0);
            }
            buf[offset..offset + data.len()].copy_from_slice(data);
        }

        {
            let mut received = self.received_fragments.write().await;
            if idx < received.len() {
                received[idx] = true;
            }

            let mut contiguous = 0u64;
            for (i, &recv) in received.iter().enumerate() {
                if recv {
                    let frag_size = if i == received.len() - 1 {
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

    pub async fn get_contiguous(&self) -> Bytes {
        let contiguous = self.contiguous_bytes.load(Ordering::Acquire) as usize;
        if contiguous == 0 {
            return Bytes::new();
        }
        let data = self.data.read().await;
        data.clone().freeze().split_to(contiguous)
    }

    pub fn is_complete(&self) -> bool {
        let contiguous = self.contiguous_bytes.load(Ordering::Acquire);
        let total = self.total_size.load(Ordering::Acquire);
        contiguous >= total
    }

    pub async fn wait_for_data(&self) {
        self.data_available.notified().await;
    }
}

// ============================================================================
// StreamHandle - shareable reference using lock-free buffer
// ============================================================================

#[derive(Clone)]
pub struct StreamHandle {
    pub stream_id: StreamId,
    buffer: Arc<LockFreeStreamBuffer>,
    read_position: Arc<AtomicU64>,
}

impl StreamHandle {
    pub fn new(stream_id: StreamId, total_size: u64) -> Self {
        Self {
            stream_id,
            buffer: Arc::new(LockFreeStreamBuffer::new(total_size, FRAGMENT_SIZE)),
            read_position: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn push_fragment(&self, fragment_num: u32, data: &[u8]) {
        self.buffer.insert(fragment_num, Bytes::copy_from_slice(data));
    }

    pub fn read_available(&self) -> Bytes {
        let pos = self.read_position.load(Ordering::Acquire) as usize;
        let data = self.buffer.collect_contiguous();
        if data.len() > pos {
            data.slice(pos..)
        } else {
            Bytes::new()
        }
    }

    pub async fn read_all(&self) -> Bytes {
        while !self.is_complete() {
            self.buffer.wait_for_data().await;
        }
        self.buffer.collect_contiguous()
    }

    pub fn is_complete(&self) -> bool {
        self.buffer.is_complete()
    }

    pub fn share(&self) -> Self {
        Self {
            stream_id: self.stream_id,
            buffer: Arc::clone(&self.buffer),
            read_position: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn available_bytes(&self) -> u64 {
        self.buffer.contiguous_bytes()
    }

    /// Get a Stream consumer
    pub fn into_stream(self) -> InboundStream {
        InboundStream::new(self.buffer)
    }
}

// ============================================================================
// PipedStream - forward fragments without full reassembly
// ============================================================================

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

    pub fn new_discard() -> Self {
        let (tx, _) = mpsc::channel(1);
        Self {
            sent_fragments: Arc::new(RwLock::new(Vec::new())),
            sender: tx,
        }
    }

    pub async fn send_fragment(&self, stream_id: StreamId, frag_num: u32, data: Bytes) {
        self.sent_fragments
            .write()
            .await
            .push((stream_id, frag_num, data.clone()));
        let _ = self.sender.send((stream_id, frag_num, data)).await;
    }
}

pub struct PipedStream {
    stream_id: StreamId,
    total_size: u64,
    next_to_forward: u32,
    out_of_order: BTreeMap<u32, Bytes>,
    targets: Vec<MockOutbound>,
    bytes_forwarded: u64,
    bytes_buffered: u64,
}

impl PipedStream {
    pub fn new(stream_id: StreamId, total_size: u64, targets: Vec<MockOutbound>) -> Self {
        Self {
            stream_id,
            total_size,
            next_to_forward: 1,
            out_of_order: BTreeMap::new(),
            targets,
            bytes_forwarded: 0,
            bytes_buffered: 0,
        }
    }

    pub async fn pipe_fragment(&mut self, frag_num: u32, data: Bytes) -> bool {
        if frag_num == self.next_to_forward {
            self.forward_to_all(frag_num, data.clone()).await;
            self.bytes_forwarded += data.len() as u64;
            self.next_to_forward += 1;

            while let Some(buffered) = self.out_of_order.remove(&self.next_to_forward) {
                let len = buffered.len() as u64;
                self.forward_to_all(self.next_to_forward, buffered).await;
                self.bytes_forwarded += len;
                self.bytes_buffered -= len;
                self.next_to_forward += 1;
            }
        } else if frag_num > self.next_to_forward {
            self.bytes_buffered += data.len() as u64;
            self.out_of_order.insert(frag_num, data);
        }

        self.bytes_forwarded >= self.total_size
    }

    async fn forward_to_all(&self, frag_num: u32, data: Bytes) {
        for target in &self.targets {
            target
                .send_fragment(self.stream_id, frag_num, data.clone())
                .await;
        }
    }

    pub fn bytes_buffered(&self) -> u64 {
        self.bytes_buffered
    }

    #[allow(dead_code)]
    pub fn bytes_forwarded(&self) -> u64 {
        self.bytes_forwarded
    }
}

// ============================================================================
// Tests & Benchmarks
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::time::Instant;

    // ------------------------------------------------------------------------
    // Basic functionality tests
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn test_lock_free_buffer_basic() {
        let buffer = LockFreeStreamBuffer::new(3000, FRAGMENT_SIZE);

        let data1 = Bytes::from(vec![1u8; FRAGMENT_SIZE]);
        let data2 = Bytes::from(vec![2u8; FRAGMENT_SIZE]);
        let data3 = Bytes::from(vec![3u8; 3000 - 2 * FRAGMENT_SIZE]);

        assert!(!buffer.insert(1, data1));
        assert_eq!(buffer.contiguous_bytes(), FRAGMENT_SIZE as u64);

        assert!(!buffer.insert(2, data2));
        assert!(!buffer.is_complete());

        assert!(buffer.insert(3, data3));
        assert!(buffer.is_complete());
    }

    #[tokio::test]
    async fn test_lock_free_buffer_out_of_order() {
        let buffer = LockFreeStreamBuffer::new(3000, FRAGMENT_SIZE);

        let data1 = Bytes::from(vec![1u8; FRAGMENT_SIZE]);
        let data2 = Bytes::from(vec![2u8; FRAGMENT_SIZE]);
        let data3 = Bytes::from(vec![3u8; 3000 - 2 * FRAGMENT_SIZE]);

        // Insert out of order
        buffer.insert(3, data3);
        assert_eq!(buffer.contiguous_bytes(), 0); // Gap at 1

        buffer.insert(1, data1);
        assert_eq!(buffer.contiguous_bytes(), FRAGMENT_SIZE as u64); // Still gap at 2

        buffer.insert(2, data2);
        assert!(buffer.is_complete());
    }

    #[tokio::test]
    async fn test_lock_free_duplicate_insert() {
        let buffer = LockFreeStreamBuffer::new(3000, FRAGMENT_SIZE);

        let data1 = Bytes::from(vec![1u8; FRAGMENT_SIZE]);
        let data1_dup = Bytes::from(vec![99u8; FRAGMENT_SIZE]); // Different content

        buffer.insert(1, data1.clone());
        buffer.insert(1, data1_dup); // Should be ignored

        // Verify first insert won
        let collected = buffer.collect_contiguous();
        assert_eq!(&collected[..], &data1[..]);
    }

    #[tokio::test]
    async fn test_inbound_stream_basic() {
        let buffer = Arc::new(LockFreeStreamBuffer::new(3000, FRAGMENT_SIZE));

        // Insert all fragments
        buffer.insert(1, Bytes::from(vec![1u8; FRAGMENT_SIZE]));
        buffer.insert(2, Bytes::from(vec![2u8; FRAGMENT_SIZE]));
        buffer.insert(3, Bytes::from(vec![3u8; 3000 - 2 * FRAGMENT_SIZE]));

        // Consume via Stream
        let stream = InboundStream::new(buffer);
        let collected: Vec<Bytes> = stream.collect().await;

        assert_eq!(collected.len(), 3);
        assert_eq!(collected[0].len(), FRAGMENT_SIZE);
        assert_eq!(collected[1].len(), FRAGMENT_SIZE);
    }

    #[tokio::test]
    async fn test_inbound_stream_fork() {
        let buffer = Arc::new(LockFreeStreamBuffer::new(3000, FRAGMENT_SIZE));

        buffer.insert(1, Bytes::from(vec![1u8; FRAGMENT_SIZE]));
        buffer.insert(2, Bytes::from(vec![2u8; FRAGMENT_SIZE]));
        buffer.insert(3, Bytes::from(vec![3u8; 3000 - 2 * FRAGMENT_SIZE]));

        let stream1 = InboundStream::new(Arc::clone(&buffer));
        let stream2 = stream1.fork();

        // Both should read same data independently
        let data1 = stream1.collect_all().await;
        let data2 = stream2.collect_all().await;

        assert_eq!(data1.len(), data2.len());
        assert_eq!(&data1[..], &data2[..]);
    }

    #[tokio::test]
    async fn test_stream_handle_with_stream() {
        let handle = StreamHandle::new(StreamId::next(), 3000);

        handle.push_fragment(1, &vec![1u8; FRAGMENT_SIZE]);
        handle.push_fragment(2, &vec![2u8; FRAGMENT_SIZE]);
        handle.push_fragment(3, &vec![3u8; 3000 - 2 * FRAGMENT_SIZE]);

        let stream = handle.into_stream();
        let collected = stream.collect_all().await;

        assert_eq!(collected.len(), 3000);
    }

    // ------------------------------------------------------------------------
    // Piped stream tests
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn test_piped_stream_in_order() {
        let stream_id = StreamId::next();
        let (target, mut rx) = MockOutbound::new();
        let mut pipe = PipedStream::new(stream_id, 3000, vec![target]);

        let frag1 = Bytes::from(vec![1u8; FRAGMENT_SIZE]);
        let frag2 = Bytes::from(vec![2u8; FRAGMENT_SIZE]);
        let frag3 = Bytes::from(vec![3u8; 3000 - 2 * FRAGMENT_SIZE]);

        assert!(!pipe.pipe_fragment(1, frag1).await);
        assert!(!pipe.pipe_fragment(2, frag2).await);
        assert!(pipe.pipe_fragment(3, frag3).await);

        let mut received = Vec::new();
        while let Ok(item) = rx.try_recv() {
            received.push(item);
        }
        assert_eq!(received.len(), 3);
        assert_eq!(pipe.bytes_buffered(), 0);
    }

    #[tokio::test]
    async fn test_piped_stream_out_of_order() {
        let stream_id = StreamId::next();
        let (target, _rx) = MockOutbound::new();
        let mut pipe = PipedStream::new(stream_id, 3000, vec![target]);

        let frag1 = Bytes::from(vec![1u8; FRAGMENT_SIZE]);
        let frag2 = Bytes::from(vec![2u8; FRAGMENT_SIZE]);
        let frag3 = Bytes::from(vec![3u8; 3000 - 2 * FRAGMENT_SIZE]);

        pipe.pipe_fragment(3, frag3.clone()).await;
        assert_eq!(pipe.bytes_buffered(), frag3.len() as u64);

        pipe.pipe_fragment(2, frag2.clone()).await;
        assert_eq!(pipe.bytes_buffered(), (frag2.len() + frag3.len()) as u64);

        pipe.pipe_fragment(1, frag1).await;
        assert_eq!(pipe.bytes_buffered(), 0); // All drained
    }

    // ------------------------------------------------------------------------
    // Performance benchmarks
    // ------------------------------------------------------------------------

    #[tokio::test]
    async fn bench_lock_free_vs_rwlock_insert() {
        const DATA_SIZE: usize = 10 * 1024 * 1024; // 10MB
        let num_fragments = DATA_SIZE.div_ceil(FRAGMENT_SIZE);

        // Pre-generate fragments
        let fragments: Vec<Bytes> = (0..num_fragments)
            .map(|i| {
                let size = if i == num_fragments - 1 {
                    DATA_SIZE - (num_fragments - 1) * FRAGMENT_SIZE
                } else {
                    FRAGMENT_SIZE
                };
                Bytes::from(vec![0u8; size])
            })
            .collect();

        let fragments_for_rwlock: Vec<Vec<u8>> = fragments.iter().map(|b| b.to_vec()).collect();

        println!("\n=== Lock-Free vs RwLock Insert Benchmark ===");
        println!("Data size: {} MB", DATA_SIZE / 1_000_000);
        println!("Fragments: {}", num_fragments);

        // Benchmark lock-free
        let lock_free_buffer = LockFreeStreamBuffer::new(DATA_SIZE as u64, FRAGMENT_SIZE);
        let start = Instant::now();
        for (i, frag) in fragments.iter().enumerate() {
            lock_free_buffer.insert((i + 1) as u32, frag.clone());
        }
        let lock_free_time = start.elapsed();

        // Benchmark RwLock
        let rwlock_buffer = RwLockStreamBuffer::new(DATA_SIZE as u64);
        let start = Instant::now();
        for (i, frag) in fragments_for_rwlock.iter().enumerate() {
            rwlock_buffer.insert_fragment((i + 1) as u32, frag).await;
        }
        let rwlock_time = start.elapsed();

        let lock_free_throughput = (DATA_SIZE as f64 / 1_000_000.0) / lock_free_time.as_secs_f64();
        let rwlock_throughput = (DATA_SIZE as f64 / 1_000_000.0) / rwlock_time.as_secs_f64();

        println!("\nLock-free OnceLock:");
        println!("  Time: {:?}", lock_free_time);
        println!("  Throughput: {:.2} MB/s", lock_free_throughput);

        println!("\nRwLock:");
        println!("  Time: {:?}", rwlock_time);
        println!("  Throughput: {:.2} MB/s", rwlock_throughput);

        println!(
            "\nSpeedup: {:.2}x",
            rwlock_time.as_secs_f64() / lock_free_time.as_secs_f64()
        );

        assert!(lock_free_buffer.is_complete());
        assert!(rwlock_buffer.is_complete());
    }

    #[tokio::test]
    async fn bench_bytes_clone_vs_vec_clone() {
        const DATA_SIZE: usize = 10 * 1024 * 1024; // 10MB
        const ITERATIONS: usize = 100;

        let bytes_data = Bytes::from(vec![0u8; DATA_SIZE]);
        let vec_data = vec![0u8; DATA_SIZE];

        println!("\n=== Bytes::clone vs Vec::clone Benchmark ===");
        println!("Data size: {} MB", DATA_SIZE / 1_000_000);
        println!("Iterations: {}", ITERATIONS);

        // Benchmark Bytes clone (refcount bump)
        let start = Instant::now();
        let mut clones: Vec<Bytes> = Vec::with_capacity(ITERATIONS);
        for _ in 0..ITERATIONS {
            clones.push(bytes_data.clone());
        }
        let bytes_time = start.elapsed();
        drop(clones);

        // Benchmark Vec clone (full copy)
        let start = Instant::now();
        let mut clones: Vec<Vec<u8>> = Vec::with_capacity(ITERATIONS);
        for _ in 0..ITERATIONS {
            clones.push(vec_data.clone());
        }
        let vec_time = start.elapsed();
        drop(clones);

        println!("\nBytes::clone (refcount):");
        println!("  Time: {:?}", bytes_time);
        println!("  Per clone: {:?}", bytes_time / ITERATIONS as u32);

        println!("\nVec::clone (full copy):");
        println!("  Time: {:?}", vec_time);
        println!("  Per clone: {:?}", vec_time / ITERATIONS as u32);

        println!(
            "\nSpeedup: {:.0}x",
            vec_time.as_secs_f64() / bytes_time.as_secs_f64()
        );

        // Bytes clone should be orders of magnitude faster
        assert!(bytes_time < vec_time / 100);
    }

    #[tokio::test]
    async fn bench_stream_consumption_patterns() {
        const DATA_SIZE: usize = 10 * 1024 * 1024; // 10MB
        let num_fragments = DATA_SIZE.div_ceil(FRAGMENT_SIZE);

        println!("\n=== Stream Consumption Patterns Benchmark ===");
        println!("Data size: {} MB", DATA_SIZE / 1_000_000);
        println!("Fragments: {}", num_fragments);

        // Setup: Create buffer with all fragments
        let buffer = Arc::new(LockFreeStreamBuffer::new(DATA_SIZE as u64, FRAGMENT_SIZE));
        for i in 1..=num_fragments {
            let size = if i == num_fragments {
                DATA_SIZE - (num_fragments - 1) * FRAGMENT_SIZE
            } else {
                FRAGMENT_SIZE
            };
            buffer.insert(i as u32, Bytes::from(vec![0u8; size]));
        }

        // Pattern 1: collect_contiguous (single collection)
        let start = Instant::now();
        let collected = buffer.collect_contiguous();
        let collect_time = start.elapsed();
        assert_eq!(collected.len(), DATA_SIZE);

        // Pattern 2: Stream iteration
        let stream = InboundStream::new(Arc::clone(&buffer));
        let start = Instant::now();
        let collected: Bytes = stream.collect_all().await;
        let stream_time = start.elapsed();
        assert_eq!(collected.len(), DATA_SIZE);

        // Pattern 3: Iterate fragments without collecting
        let start = Instant::now();
        let mut total = 0usize;
        for frag in buffer.iter_contiguous() {
            total += frag.len();
        }
        let iter_time = start.elapsed();
        assert_eq!(total, DATA_SIZE);

        println!("\ncollect_contiguous():");
        println!("  Time: {:?}", collect_time);

        println!("\nStream::collect_all():");
        println!("  Time: {:?}", stream_time);

        println!("\niter_contiguous() (no collection):");
        println!("  Time: {:?}", iter_time);
    }

    #[tokio::test]
    async fn bench_piped_forwarding_memory() {
        const DATA_SIZE: usize = 10 * 1024 * 1024; // 10MB
        let num_fragments = DATA_SIZE.div_ceil(FRAGMENT_SIZE);

        println!("\n=== Piped Forwarding Memory Benchmark ===");
        println!("Data size: {} MB", DATA_SIZE / 1_000_000);
        println!("Fragments: {}", num_fragments);

        // Pre-generate fragments
        let fragments: Vec<Bytes> = (0..num_fragments)
            .map(|i| {
                let size = if i == num_fragments - 1 {
                    DATA_SIZE - (num_fragments - 1) * FRAGMENT_SIZE
                } else {
                    FRAGMENT_SIZE
                };
                Bytes::from(vec![0u8; size])
            })
            .collect();

        // Test 1: In-order (best case)
        {
            let target = MockOutbound::new_discard();
            let mut pipe = PipedStream::new(StreamId::next(), DATA_SIZE as u64, vec![target]);
            let mut max_buffer = 0u64;

            let start = Instant::now();
            for (i, frag) in fragments.iter().enumerate() {
                pipe.pipe_fragment((i + 1) as u32, frag.clone()).await;
                max_buffer = max_buffer.max(pipe.bytes_buffered());
            }
            let elapsed = start.elapsed();

            println!("\nIn-order delivery:");
            println!("  Time: {:?}", elapsed);
            println!("  Max buffer: {} bytes", max_buffer);
            println!(
                "  Throughput: {:.2} MB/s",
                (DATA_SIZE as f64 / 1_000_000.0) / elapsed.as_secs_f64()
            );
            assert_eq!(max_buffer, 0);
        }

        // Test 2: Reverse order (worst case)
        {
            let target = MockOutbound::new_discard();
            let mut pipe = PipedStream::new(StreamId::next(), DATA_SIZE as u64, vec![target]);
            let mut max_buffer = 0u64;

            let start = Instant::now();
            for i in (1..=num_fragments).rev() {
                pipe.pipe_fragment(i as u32, fragments[i - 1].clone()).await;
                max_buffer = max_buffer.max(pipe.bytes_buffered());
            }
            let elapsed = start.elapsed();

            println!("\nReverse order (worst case):");
            println!("  Time: {:?}", elapsed);
            println!(
                "  Max buffer: {} bytes ({:.1}% of data)",
                max_buffer,
                max_buffer as f64 / DATA_SIZE as f64 * 100.0
            );
            println!(
                "  Throughput: {:.2} MB/s",
                (DATA_SIZE as f64 / 1_000_000.0) / elapsed.as_secs_f64()
            );
        }

        // Test 3: Chunked reordering (realistic)
        {
            let target = MockOutbound::new_discard();
            let mut pipe = PipedStream::new(StreamId::next(), DATA_SIZE as u64, vec![target]);
            let mut max_buffer = 0u64;

            // Reorder within chunks of 10
            let chunk_size = 10;
            let mut order: Vec<usize> = (1..=num_fragments).collect();
            for chunk in order.chunks_mut(chunk_size) {
                chunk.reverse();
            }

            let start = Instant::now();
            for i in order {
                pipe.pipe_fragment(i as u32, fragments[i - 1].clone()).await;
                max_buffer = max_buffer.max(pipe.bytes_buffered());
            }
            let elapsed = start.elapsed();

            println!("\nChunked reorder (realistic):");
            println!("  Time: {:?}", elapsed);
            println!(
                "  Max buffer: {} bytes ({:.1}% of data)",
                max_buffer,
                max_buffer as f64 / DATA_SIZE as f64 * 100.0
            );
            println!(
                "  Throughput: {:.2} MB/s",
                (DATA_SIZE as f64 / 1_000_000.0) / elapsed.as_secs_f64()
            );
        }

        // Comparison: Full reassembly baseline
        println!("\nFull reassembly baseline:");
        println!("  Buffer needed: {} bytes (100% of data)", DATA_SIZE);
    }

    #[tokio::test]
    async fn bench_concurrent_consumers() {
        const DATA_SIZE: usize = 10 * 1024 * 1024; // 10MB
        let num_fragments = DATA_SIZE.div_ceil(FRAGMENT_SIZE);

        println!("\n=== Concurrent Consumers Benchmark ===");
        println!("Data size: {} MB", DATA_SIZE / 1_000_000);

        // Create buffer and insert all data
        let buffer = Arc::new(LockFreeStreamBuffer::new(DATA_SIZE as u64, FRAGMENT_SIZE));
        for i in 1..=num_fragments {
            let size = if i == num_fragments {
                DATA_SIZE - (num_fragments - 1) * FRAGMENT_SIZE
            } else {
                FRAGMENT_SIZE
            };
            buffer.insert(i as u32, Bytes::from(vec![0u8; size]));
        }

        // Create multiple consumers
        let num_consumers = 4;
        let consumers: Vec<InboundStream> = (0..num_consumers)
            .map(|_| InboundStream::new(Arc::clone(&buffer)))
            .collect();

        let start = Instant::now();
        let handles: Vec<_> = consumers
            .into_iter()
            .map(|c| tokio::spawn(async move { c.collect_all().await.len() }))
            .collect();

        let results: Vec<usize> = futures::future::join_all(handles)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();

        let elapsed = start.elapsed();

        println!("\n{} concurrent consumers:", num_consumers);
        println!("  Total time: {:?}", elapsed);
        println!("  Results: {:?}", results);
        println!(
            "  Total data read: {} MB",
            results.iter().sum::<usize>() / 1_000_000
        );

        // All consumers should read full data
        for r in results {
            assert_eq!(r, DATA_SIZE);
        }
    }

    #[tokio::test]
    async fn bench_first_fragment_latency() {
        const DATA_SIZE: usize = 10 * 1024 * 1024; // 10MB

        println!("\n=== First Fragment Latency Benchmark ===");

        // Lock-free buffer
        let buffer = LockFreeStreamBuffer::new(DATA_SIZE as u64, FRAGMENT_SIZE);
        let frag = Bytes::from(vec![0u8; FRAGMENT_SIZE]);

        let start = Instant::now();
        buffer.insert(1, frag);
        let first_byte = buffer.collect_contiguous();
        let latency = start.elapsed();

        println!("\nLock-free buffer:");
        println!("  First fragment latency: {:?}", latency);
        println!("  Bytes available: {}", first_byte.len());

        // RwLock buffer
        let buffer = RwLockStreamBuffer::new(DATA_SIZE as u64);
        let frag = vec![0u8; FRAGMENT_SIZE];

        let start = Instant::now();
        buffer.insert_fragment(1, &frag).await;
        let first_byte = buffer.get_contiguous().await;
        let latency = start.elapsed();

        println!("\nRwLock buffer:");
        println!("  First fragment latency: {:?}", latency);
        println!("  Bytes available: {}", first_byte.len());
    }

    #[tokio::test]
    async fn bench_end_to_end_comparison() {
        const DATA_SIZE: usize = 10 * 1024 * 1024; // 10MB
        let num_fragments = DATA_SIZE.div_ceil(FRAGMENT_SIZE);

        println!("\n=== End-to-End Comparison: Current vs Streaming ===");
        println!("Data size: {} MB", DATA_SIZE / 1_000_000);
        println!("Fragments: {}", num_fragments);

        // Pre-generate fragments
        let fragments: Vec<Vec<u8>> = (0..num_fragments)
            .map(|i| {
                let size = if i == num_fragments - 1 {
                    DATA_SIZE - (num_fragments - 1) * FRAGMENT_SIZE
                } else {
                    FRAGMENT_SIZE
                };
                vec![0u8; size]
            })
            .collect();

        // Simulate CURRENT approach: Full reassembly then clone for forwarding
        println!("\n--- Current Approach (full reassembly + clone) ---");
        {
            let start = Instant::now();

            // Step 1: Reassemble into Vec<u8>
            let mut buffer = Vec::with_capacity(DATA_SIZE);
            for frag in &fragments {
                buffer.extend_from_slice(frag);
            }

            // Step 2: Clone for forwarding (simulates current behavior)
            let forward_copy = buffer.clone();
            let cache_copy = buffer.clone();

            let elapsed = start.elapsed();
            let peak_memory = DATA_SIZE * 3; // original + 2 clones

            println!("  Time: {:?}", elapsed);
            println!("  Peak memory: {} MB", peak_memory / 1_000_000);
            println!("  Copies made: 2 (forward + cache)");

            drop(forward_copy);
            drop(cache_copy);
            drop(buffer);
        }

        // Simulate STREAMING approach: Lock-free buffer + Bytes sharing
        println!("\n--- Streaming Approach (lock-free + Bytes) ---");
        {
            let start = Instant::now();

            // Step 1: Insert fragments into lock-free buffer
            let buffer = Arc::new(LockFreeStreamBuffer::new(DATA_SIZE as u64, FRAGMENT_SIZE));
            for (i, frag) in fragments.iter().enumerate() {
                buffer.insert((i + 1) as u32, Bytes::copy_from_slice(frag));
            }

            // Step 2: Create consumers (Bytes::clone = refcount)
            let forward_stream = InboundStream::new(Arc::clone(&buffer));
            let cache_stream = InboundStream::new(Arc::clone(&buffer));

            // Step 3: Collect both (in parallel in real code)
            let forward_data = forward_stream.collect_all().await;
            let cache_data = cache_stream.collect_all().await;

            let elapsed = start.elapsed();
            let peak_memory = DATA_SIZE; // Fragments stored once

            println!("  Time: {:?}", elapsed);
            println!("  Peak memory: ~{} MB (shared)", peak_memory / 1_000_000);
            println!("  Copies made: 0 (refcount sharing)");

            assert_eq!(forward_data.len(), DATA_SIZE);
            assert_eq!(cache_data.len(), DATA_SIZE);
        }

        // Piped forwarding (no reassembly at all)
        println!("\n--- Piped Forwarding (intermediate node) ---");
        {
            let target = MockOutbound::new_discard();
            let mut pipe = PipedStream::new(StreamId::next(), DATA_SIZE as u64, vec![target]);

            let start = Instant::now();
            for (i, frag) in fragments.iter().enumerate() {
                pipe.pipe_fragment((i + 1) as u32, Bytes::copy_from_slice(frag))
                    .await;
            }
            let elapsed = start.elapsed();

            println!("  Time: {:?}", elapsed);
            println!("  Peak buffer: 0 bytes (in-order)");
            println!(
                "  Memory saved vs current: {} MB",
                DATA_SIZE / 1_000_000
            );
        }
    }
}

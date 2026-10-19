use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::Ordering;

use crate::error::{Error, Result};
use portable_atomic::{AtomicBool, AtomicUsize};
use tokio::sync::{Mutex, MutexGuard, Semaphore, SemaphorePermit};
use util::sync::RwLock;

use crate::chunk::chunk_payload_data::ChunkPayloadData;

// TODO: benchmark performance between multiple Atomic+Mutex vs one Mutex<PendingQueueInternal>

// Some tests push a lot of data before starting to process any data...
#[cfg(test)]
const QUEUE_BYTES_LIMIT: usize = 128 * 1024 * 1024;
/// Send-admission budget. Multiple complete FILE messages and TEXT
/// can coexist so message-boundary stream scheduling can actually take effect.
#[cfg(not(test))]
const QUEUE_BYTES_LIMIT: usize = 2_000_000;
/// Basic queue for either ordered or unordered chunks.
pub(crate) type PendingBaseQueue = VecDeque<ChunkPayloadData>;

/// The negotiated non-interleaved path selects a stream at message
/// boundaries. It never interrupts the fragments of an admitted message.
#[derive(Debug, Default)]
struct OrderedQueue {
    streams: BTreeMap<u16, PendingBaseQueue>,
    ready: BTreeSet<(u64, u16)>,
    current: Option<u16>,
    virtual_time: u64,
}

impl OrderedQueue {
    fn extend(&mut self, chunks: Vec<ChunkPayloadData>) {
        for chunk in chunks {
            let id = chunk.stream_identifier;
            let queue = self.streams.entry(id).or_default();
            if queue.is_empty() && self.current != Some(id) {
                self.ready.insert((self.virtual_time + 1, id));
            }
            queue.push_back(chunk);
        }
    }

    fn front(&mut self) -> Option<&ChunkPayloadData> {
        // Pin the selection through the writer's cwnd/rwnd check and pop.
        // Concurrent admission of a lower-ID stream must not change its head.
        let id = match self.current {
            Some(id) => id,
            None => {
                let (finish, id) = self.ready.pop_first()?;
                self.virtual_time = self.virtual_time.max(finish);
                self.current = Some(id);
                id
            }
        };
        self.streams.get(&id)?.front()
    }

    fn pop_front(&mut self) -> Option<ChunkPayloadData> {
        let id = match self.current {
            Some(id) => id,
            None => {
                let (finish, id) = self.ready.pop_first()?;
                self.virtual_time = self.virtual_time.max(finish);
                self.current = Some(id);
                id
            }
        };
        let queue = self.streams.get_mut(&id)?;
        let chunk = queue.pop_front()?;
        if chunk.ending_fragment {
            self.current = None;
            if queue.is_empty() {
                self.streams.remove(&id);
            } else {
                self.ready.insert((self.virtual_time + 1, id));
            }
        }
        Some(chunk)
    }
}

/// A queue for both ordered and unordered chunks.
#[derive(Debug)]
pub(crate) struct PendingQueue {
    // These two fields limit appending bytes to the queue
    // This two step process is necessary because
    // A) We need backpressure which the semaphore applies by limiting the total amount of bytes via the permits
    // B) The chunks of one fragmented message need to be put in direct sequence into the queue which the lock guarantees
    //
    // The semaphore is not inside the lock because the permits need to be returned without needing a lock on the semaphore
    semaphore_lock: Mutex<()>,
    semaphore: Semaphore,

    unordered_queue: RwLock<PendingBaseQueue>,
    ordered_queue: RwLock<OrderedQueue>,
    queue_len: AtomicUsize,
    n_bytes: AtomicUsize,
    selected: AtomicBool,
    unordered_is_selected: AtomicBool,
}

impl Default for PendingQueue {
    fn default() -> Self {
        PendingQueue::new()
    }
}

impl PendingQueue {
    pub(crate) fn new() -> Self {
        Self {
            semaphore_lock: Mutex::default(),
            semaphore: Semaphore::new(QUEUE_BYTES_LIMIT),
            unordered_queue: Default::default(),
            ordered_queue: Default::default(),
            queue_len: Default::default(),
            n_bytes: Default::default(),
            selected: Default::default(),
            unordered_is_selected: Default::default(),
        }
    }

    /// Reserve before assigning a stream sequence number. A large message
    /// exclusively occupies the budget until its last fragment leaves pending;
    /// memory is bounded by max(queue budget, negotiated maximum message size).
    pub(crate) async fn reserve(&self, bytes: usize) -> Result<PendingReservation<'_>> {
        let guard = self.semaphore_lock.lock().await;
        let permit = self
            .semaphore
            .acquire_many(bytes.min(QUEUE_BYTES_LIMIT) as u32)
            .await
            .map_err(|_| Error::ErrStreamClosed)?;
        Ok(PendingReservation {
            queue: self,
            _guard: guard,
            permit,
        })
    }

    pub(crate) fn close(&self) {
        self.semaphore.close();
    }

    pub(crate) async fn push(&self, chunk: ChunkPayloadData) -> Result<()> {
        self.reserve(chunk.user_data.len())
            .await?
            .commit(vec![chunk]);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn append(&self, chunks: Vec<ChunkPayloadData>) {
        self.append_and_wake(chunks, || {}).await.unwrap();
    }

    #[cfg(test)]
    pub(crate) async fn append_and_wake(
        &self,
        chunks: Vec<ChunkPayloadData>,
        mut wake: impl FnMut(),
    ) -> Result<()> {
        let bytes = chunks.iter().map(|c| c.user_data.len()).sum();
        self.reserve(bytes).await?.commit(chunks);
        wake();
        Ok(())
    }

    /// Assumes that A) enough permits have been acquired and forget from the semaphore and that the semaphore_lock is held
    fn append_unlimited(&self, chunks: Vec<ChunkPayloadData>, total_user_data_len: usize) {
        let chunks_len = chunks.len();
        let unordered = chunks
            .first()
            .expect("chunks to not be empty because of the above check")
            .unordered;
        if unordered {
            let mut unordered_queue = self.unordered_queue.write();
            assert!(
                chunks.iter().all(|c| c.unordered),
                "expected all chunks to be unordered"
            );
            unordered_queue.extend(chunks);
            self.n_bytes
                .fetch_add(total_user_data_len, Ordering::SeqCst);
            self.queue_len.fetch_add(chunks_len, Ordering::SeqCst);
        } else {
            let mut ordered_queue = self.ordered_queue.write();
            assert!(
                chunks.iter().all(|c| !c.unordered),
                "expected all chunks to be ordered"
            );
            ordered_queue.extend(chunks);
            self.n_bytes
                .fetch_add(total_user_data_len, Ordering::SeqCst);
            self.queue_len.fetch_add(chunks_len, Ordering::SeqCst);
        }
    }

    pub(crate) fn peek(&self) -> Option<ChunkPayloadData> {
        if self.selected.load(Ordering::SeqCst) {
            if self.unordered_is_selected.load(Ordering::SeqCst) {
                let unordered_queue = self.unordered_queue.read();
                return unordered_queue.front().cloned();
            } else {
                let mut ordered_queue = self.ordered_queue.write();
                return ordered_queue.front().cloned();
            }
        }

        let c = {
            let unordered_queue = self.unordered_queue.read();
            unordered_queue.front().cloned()
        };

        if c.is_some() {
            return c;
        }

        let mut ordered_queue = self.ordered_queue.write();
        ordered_queue.front().cloned()
    }

    pub(crate) fn pop(
        &self,
        beginning_fragment: bool,
        unordered: bool,
    ) -> Option<ChunkPayloadData> {
        let popped = if self.selected.load(Ordering::SeqCst) {
            let popped = if self.unordered_is_selected.load(Ordering::SeqCst) {
                let mut unordered_queue = self.unordered_queue.write();
                unordered_queue.pop_front()
            } else {
                let mut ordered_queue = self.ordered_queue.write();
                ordered_queue.pop_front()
            };
            if let Some(p) = &popped {
                if p.ending_fragment {
                    self.selected.store(false, Ordering::SeqCst);
                }
            }
            popped
        } else {
            if !beginning_fragment {
                return None;
            }
            if unordered {
                let popped = {
                    let mut unordered_queue = self.unordered_queue.write();
                    unordered_queue.pop_front()
                };
                if let Some(p) = &popped {
                    if !p.ending_fragment {
                        self.selected.store(true, Ordering::SeqCst);
                        self.unordered_is_selected.store(true, Ordering::SeqCst);
                    }
                }
                popped
            } else {
                let popped = {
                    let mut ordered_queue = self.ordered_queue.write();
                    ordered_queue.pop_front()
                };
                if let Some(p) = &popped {
                    if !p.ending_fragment {
                        self.selected.store(true, Ordering::SeqCst);
                        self.unordered_is_selected.store(false, Ordering::SeqCst);
                    }
                }
                popped
            }
        };

        if let Some(p) = &popped {
            let user_data_len = p.user_data.len();
            self.n_bytes.fetch_sub(user_data_len, Ordering::SeqCst);
            self.queue_len.fetch_sub(1, Ordering::SeqCst);
            self.semaphore.add_permits(p.pending_queue_credit);
        }

        popped
    }

    pub(crate) fn get_num_bytes(&self) -> usize {
        self.n_bytes.load(Ordering::SeqCst)
    }

    pub(crate) fn len(&self) -> usize {
        self.queue_len.load(Ordering::SeqCst)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// There is no await between assigning SSN and committing all its fragments.
pub(crate) struct PendingReservation<'a> {
    queue: &'a PendingQueue,
    _guard: MutexGuard<'a, ()>,
    permit: SemaphorePermit<'a>,
}

impl PendingReservation<'_> {
    pub(crate) fn commit(self, mut chunks: Vec<ChunkPayloadData>) {
        if chunks.is_empty() {
            return;
        }
        let total = chunks.iter().map(|c| c.user_data.len()).sum();
        for chunk in &mut chunks {
            chunk.pending_queue_credit = 0;
        }
        chunks.last_mut().unwrap().pending_queue_credit = self.permit.num_permits();
        self.permit.forget();
        self.queue.append_unlimited(chunks, total);
    }
}

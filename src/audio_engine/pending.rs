//! The queue between the player and an engine's worker thread. Discarding
//! output for a previous file and flushing before a blocking send happen here.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};

#[derive(Clone, Default)]
struct Generation(Arc<AtomicU64>);

impl Generation {
    fn current(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}

enum Message {
    Data(u32),
    /// Acknowledged by the worker once everything queued earlier is handled
    /// and the engine is unlocked.
    Flush(SyncSender<()>),
}

/// Messages from the player, already stripped of anything queued for a
/// previous file. Closes when the output is dropped.
pub(super) struct MidiReceiver {
    rx: Receiver<(Message, u64)>,
    generation: Generation,
}

/// What the worker should do next.
pub(super) enum Next {
    Data(u32),
    /// Everything queued earlier has been handled; send on this once the
    /// engine is free for the waiting caller.
    Flush(SyncSender<()>),
    /// Nothing queued right now (only without blocking).
    Empty,
    /// The output was dropped.
    Closed,
}

impl MidiReceiver {
    /// Next item, skipping output queued for a previous file. The generation
    /// is compared on receipt, so an invalidation while the engine was busy
    /// still discards what was queued before it.
    pub fn next(&self, block: bool) -> Next {
        loop {
            let (message, generation) = if block {
                match self.rx.recv() {
                    Ok(entry) => entry,
                    Err(_) => return Next::Closed,
                }
            } else {
                match self.rx.try_recv() {
                    Ok(entry) => entry,
                    Err(TryRecvError::Empty) => return Next::Empty,
                    Err(TryRecvError::Disconnected) => return Next::Closed,
                }
            };
            match message {
                Message::Data(data) if generation == self.generation.current() => {
                    return Next::Data(data);
                }
                Message::Data(_) => {}
                Message::Flush(done) => return Next::Flush(done),
            }
        }
    }
}

/// Player side of the queue.
pub(super) struct MidiSender {
    tx: Sender<(Message, u64)>,
    generation: Generation,
}

pub(super) fn channel() -> (MidiSender, MidiReceiver) {
    let (tx, rx) = mpsc::channel();
    let generation = Generation::default();
    (
        MidiSender {
            tx,
            generation: generation.clone(),
        },
        MidiReceiver { rx, generation },
    )
}

impl MidiSender {
    pub fn send(&self, data: u32) {
        let _ = self
            .tx
            .send((Message::Data(data), self.generation.current()));
    }

    /// Wait until the engine has taken everything queued so far.
    /// `false` if the engine is gone.
    pub fn flush(&self) -> bool {
        let (done, wait) = mpsc::sync_channel(0);
        self.tx
            .send((Message::Flush(done), self.generation.current()))
            .is_ok()
            && wait.recv().is_ok()
    }

    pub fn invalidate(&self) {
        self.generation.0.fetch_add(1, Ordering::AcqRel);
    }
}

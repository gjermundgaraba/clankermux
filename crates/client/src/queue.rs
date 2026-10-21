//! Byte-bounded queues. Permits stay with queued (or in-flight) values.

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

pub(crate) struct Queued<T> {
    pub value: T,
    _permit: Option<OwnedSemaphorePermit>,
}

pub(crate) struct Sender<T> {
    tx: mpsc::UnboundedSender<Queued<T>>,
    permits: Arc<Semaphore>,
}

pub(crate) struct WeakSender<T> {
    tx: mpsc::WeakUnboundedSender<Queued<T>>,
    permits: Arc<Semaphore>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SendError {
    Full,
    Closed,
}

/// A byte-bounded receive queue. Receiving releases the item's byte budget.
/// Attachment overflow delivers a final local `ERROR too_slow`; an event
/// queue overflow ends the connection, whose `ended()` reports the loss.
pub struct Receiver<T> {
    pub(crate) rx: mpsc::UnboundedReceiver<Queued<T>>,
}

pub(crate) fn channel<T>(bytes: usize) -> (Sender<T>, Receiver<T>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (
        Sender {
            tx,
            permits: Arc::new(Semaphore::new(bytes)),
        },
        Receiver { rx },
    )
}

impl<T> Sender<T> {
    pub fn send(&self, value: T, bytes: usize) -> Result<(), SendError> {
        if self.tx.is_closed() {
            return Err(SendError::Closed);
        }
        let charge = bytes
            .checked_add(size_of::<Queued<T>>())
            .and_then(|n| u32::try_from(n).ok())
            .ok_or(SendError::Full)?;
        let permit = self
            .permits
            .clone()
            .try_acquire_many_owned(charge)
            .map_err(|_| SendError::Full)?;
        self.tx
            .send(Queued {
                value,
                _permit: Some(permit),
            })
            .map_err(|_| SendError::Closed)
    }

    /// One reserved ending, after the bounded prefix. Consumes the sender
    /// so overflow cannot grow a second queue of loss notifications.
    pub fn end(self, value: T) {
        let _ = self.tx.send(Queued {
            value,
            _permit: None,
        });
    }

    pub fn downgrade(&self) -> WeakSender<T> {
        WeakSender {
            tx: self.tx.downgrade(),
            permits: self.permits.clone(),
        }
    }
}

impl<T> WeakSender<T> {
    pub fn send(&self, value: T, bytes: usize) -> Result<(), SendError> {
        let tx = self.tx.upgrade().ok_or(SendError::Closed)?;
        Sender {
            tx,
            permits: self.permits.clone(),
        }
        .send(value, bytes)
    }
}

impl<T> Receiver<T> {
    pub async fn recv(&mut self) -> Option<T> {
        self.rx.recv().await.map(|queued| queued.value)
    }

    pub fn try_recv(&mut self) -> Result<T, mpsc::error::TryRecvError> {
        self.rx.try_recv().map(|queued| queued.value)
    }

    pub fn len(&self) -> usize {
        self.rx.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rx.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn budgets_include_in_flight_values_and_empty_items() {
        let charge = 10 + size_of::<Queued<Vec<u8>>>();
        let (tx, mut rx) = channel(2 * charge);
        tx.send(vec![0; 10], 10).unwrap();
        tx.send(vec![0; 10], 10).unwrap();
        assert_eq!(tx.send(vec![0; 10], 10), Err(SendError::Full));
        let in_flight = rx.rx.recv().await.unwrap();
        assert_eq!(tx.send(vec![0; 10], 10), Err(SendError::Full));
        drop(in_flight);
        tx.send(vec![0; 10], 10).unwrap();
        rx.recv().await.unwrap();
        tx.send(vec![0; 10], 10).unwrap();
        drop(rx);
        assert_eq!(tx.send(vec![], 0), Err(SendError::Closed));

        let (tx, mut rx) = channel(size_of::<Queued<()>>());
        tx.send((), 0).unwrap();
        assert_eq!(tx.send((), 0), Err(SendError::Full));
        tx.end(());
        assert_eq!(rx.recv().await, Some(()));
        assert_eq!(rx.recv().await, Some(()));
        assert_eq!(rx.recv().await, None);
    }
}

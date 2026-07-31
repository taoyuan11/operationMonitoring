use std::{error::Error, fmt};

use tokio::sync::{mpsc, watch};

use crate::models::AgentInbound;

#[derive(Clone)]
pub(crate) struct AgentEventSender {
    tx: mpsc::Sender<AgentInbound>,
    disconnect_tx: watch::Sender<bool>,
}

impl AgentEventSender {
    pub(crate) fn channel(
        capacity: usize,
    ) -> (Self, mpsc::Receiver<AgentInbound>, watch::Receiver<bool>) {
        let (tx, rx) = mpsc::channel(capacity);
        let (disconnect_tx, disconnect_rx) = watch::channel(false);
        (Self { tx, disconnect_tx }, rx, disconnect_rx)
    }

    pub(crate) fn send(&self, event: AgentInbound) -> Result<(), AgentEventSendError> {
        self.tx.try_send(event).map_err(|error| {
            self.fail(match error {
                mpsc::error::TrySendError::Full(_) => AgentEventSendError::Full,
                mpsc::error::TrySendError::Closed(_) => AgentEventSendError::Closed,
            })
        })
    }

    pub(crate) async fn send_async(&self, event: AgentInbound) -> Result<(), AgentEventSendError> {
        self.tx
            .send(event)
            .await
            .map_err(|_| self.fail(AgentEventSendError::Closed))
    }

    pub(crate) fn blocking_send(&self, event: AgentInbound) -> Result<(), AgentEventSendError> {
        self.tx
            .blocking_send(event)
            .map_err(|_| self.fail(AgentEventSendError::Closed))
    }

    fn fail(&self, error: AgentEventSendError) -> AgentEventSendError {
        self.disconnect_tx.send_replace(true);
        error
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AgentEventSendError {
    Full,
    Closed,
}

impl fmt::Display for AgentEventSendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full => formatter.write_str("agent event queue is full"),
            Self::Closed => formatter.write_str("agent event queue is closed"),
        }
    }
}

impl Error for AgentEventSendError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_overflow_requests_socket_disconnect() {
        let (tx, mut rx, disconnect_rx) = AgentEventSender::channel(1);

        assert!(tx.send(AgentInbound::Pong { now: 1 }).is_ok());
        assert_eq!(
            tx.send(AgentInbound::Pong { now: 2 }),
            Err(AgentEventSendError::Full)
        );
        assert!(*disconnect_rx.borrow());
        assert!(matches!(rx.try_recv(), Ok(AgentInbound::Pong { now: 1 })));
    }

    #[test]
    fn closed_queue_requests_socket_disconnect() {
        let (tx, rx, disconnect_rx) = AgentEventSender::channel(1);
        drop(rx);

        assert_eq!(
            tx.send(AgentInbound::Pong { now: 1 }),
            Err(AgentEventSendError::Closed)
        );
        assert!(*disconnect_rx.borrow());
    }

    #[tokio::test]
    async fn async_send_waits_for_capacity_and_preserves_order() {
        let (tx, mut rx, disconnect_rx) = AgentEventSender::channel(1);
        tx.send(AgentInbound::Pong { now: 1 }).unwrap();
        let waiting_tx = tx.clone();
        let waiting =
            tokio::spawn(async move { waiting_tx.send_async(AgentInbound::Pong { now: 2 }).await });
        tokio::task::yield_now().await;

        assert!(!waiting.is_finished());
        assert!(!*disconnect_rx.borrow());
        assert!(matches!(
            rx.recv().await,
            Some(AgentInbound::Pong { now: 1 })
        ));
        assert_eq!(waiting.await.unwrap(), Ok(()));
        assert!(matches!(
            rx.recv().await,
            Some(AgentInbound::Pong { now: 2 })
        ));
        assert!(!*disconnect_rx.borrow());
    }

    #[tokio::test]
    async fn async_send_on_a_closed_queue_requests_socket_disconnect() {
        let (tx, rx, disconnect_rx) = AgentEventSender::channel(1);
        drop(rx);

        assert_eq!(
            tx.send_async(AgentInbound::Pong { now: 1 }).await,
            Err(AgentEventSendError::Closed)
        );
        assert!(*disconnect_rx.borrow());
    }

    #[test]
    fn blocking_send_waits_for_capacity_and_preserves_order() {
        let (tx, mut rx, disconnect_rx) = AgentEventSender::channel(1);
        tx.send(AgentInbound::Pong { now: 1 }).unwrap();
        let waiting_tx = tx.clone();
        let waiting =
            std::thread::spawn(move || waiting_tx.blocking_send(AgentInbound::Pong { now: 2 }));

        assert!(matches!(
            rx.blocking_recv(),
            Some(AgentInbound::Pong { now: 1 })
        ));
        assert_eq!(waiting.join().unwrap(), Ok(()));
        assert!(matches!(
            rx.blocking_recv(),
            Some(AgentInbound::Pong { now: 2 })
        ));
        assert!(!*disconnect_rx.borrow());
    }

    #[test]
    fn blocked_send_returns_when_the_receiver_closes() {
        let (tx, rx, disconnect_rx) = AgentEventSender::channel(1);
        tx.send(AgentInbound::Pong { now: 1 }).unwrap();
        let waiting_tx = tx.clone();
        let waiting =
            std::thread::spawn(move || waiting_tx.blocking_send(AgentInbound::Pong { now: 2 }));
        drop(rx);

        assert_eq!(waiting.join().unwrap(), Err(AgentEventSendError::Closed));
        assert!(*disconnect_rx.borrow());
    }
}

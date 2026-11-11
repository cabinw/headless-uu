use std::sync::Weak;

use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex};
use tokio::time::Duration;

pub(crate) const ACK_INTERVAL: Duration = Duration::from_millis(200);

/// ackTimerObserver is the interface to an ack timer observer.
#[async_trait]
pub(crate) trait AckTimerObserver {
    async fn on_ack_timeout(&mut self);
}

/// Delayed ACK timer: RFC 9260 section 6.2 recommends 200 ms and requires at most
/// 500 ms. Preserve the 200 ms interval while allowing every expiry to re-arm.
/// https://www.rfc-editor.org/rfc/rfc9260.html#section-6.2
#[derive(Default, Debug)]
pub(crate) struct AckTimer<T: 'static + AckTimerObserver + Send> {
    pub(crate) timeout_observer: Weak<Mutex<T>>,
    pub(crate) interval: Duration,
    pub(crate) close_tx: Option<mpsc::Sender<()>>,
}

impl<T: 'static + AckTimerObserver + Send> AckTimer<T> {
    /// newAckTimer creates a new acknowledgement timer used to enable delayed ack.
    pub(crate) fn new(timeout_observer: Weak<Mutex<T>>, interval: Duration) -> Self {
        AckTimer {
            timeout_observer,
            interval,
            close_tx: None,
        }
    }

    /// start starts the timer.
    pub(crate) fn start(&mut self) -> bool {
        if self.is_running() {
            return false;
        }

        let (close_tx, mut close_rx) = mpsc::channel(1);
        let interval = self.interval;
        let timeout_observer = self.timeout_observer.clone();

        tokio::spawn(async move {
            let timer = tokio::time::sleep(interval);
            tokio::pin!(timer);

            tokio::select! {
                biased;
                _ = close_rx.recv() => return,
                _ = timer.as_mut() => {},
            }
            if let Some(observer) = timeout_observer.upgrade() {
                // Cancellation must remain effective after expiry, while the association
                // mutex is busy. Each start owns a separate cancellation receiver.
                let mut observer = tokio::select! {
                    biased;
                    _ = close_rx.recv() => return,
                    observer = observer.lock() => observer,
                };
                if close_rx.is_closed() {
                    return;
                }
                observer.on_ack_timeout().await;
                // Publish completion before releasing the observer mutex. A new inbound
                // packet may immediately re-arm the timer once that mutex is released.
                // Close only this generation, never a replacement timer in the observer.
                close_rx.close();
            }
        });

        self.close_tx = Some(close_tx);
        true
    }

    /// Stops the current generation; a subsequent start creates a new one.
    pub(crate) fn stop(&mut self) {
        self.close_tx.take();
    }

    /// Whether the current generation still has an open cancellation receiver.
    pub(crate) fn is_running(&self) -> bool {
        self.close_tx
            .as_ref()
            .is_some_and(|sender| !sender.is_closed())
    }
}

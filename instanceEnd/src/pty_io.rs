use std::{
    fmt,
    io::{self, Write},
    sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    thread,
};

pub(crate) struct PtyInputWriter {
    input: SyncSender<Vec<u8>>,
    failures: Receiver<io::Error>,
}

impl PtyInputWriter {
    pub(crate) fn spawn(
        mut writer: Box<dyn Write + Send>,
        queue_capacity: usize,
        thread_name: &str,
    ) -> io::Result<Self> {
        let (input, inputs) = mpsc::sync_channel::<Vec<u8>>(queue_capacity);
        let (failure_tx, failures) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name(thread_name.to_string())
            .spawn(move || {
                while let Ok(data) = inputs.recv() {
                    if let Err(error) = writer.write_all(&data).and_then(|_| writer.flush()) {
                        let _ = failure_tx.try_send(error);
                        break;
                    }
                }
            })?;
        Ok(Self { input, failures })
    }

    pub(crate) fn try_write(&self, data: Vec<u8>) -> Result<(), PtyInputSendError> {
        self.input.try_send(data).map_err(|error| match error {
            TrySendError::Full(_) => PtyInputSendError::Full,
            TrySendError::Disconnected(_) => PtyInputSendError::Closed,
        })
    }

    pub(crate) fn take_failure(&self) -> Option<io::Error> {
        match self.failures.try_recv() {
            Ok(error) => Some(error),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PtyInputSendError {
    Full,
    Closed,
}

impl fmt::Display for PtyInputSendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full => formatter.write_str("PTY input queue is full"),
            Self::Closed => formatter.write_str("PTY input writer is closed"),
        }
    }
}

impl std::error::Error for PtyInputSendError {}

#[cfg(test)]
mod tests {
    use std::{
        sync::mpsc,
        time::{Duration, Instant},
    };

    use super::*;

    struct BlockingOnceWriter {
        started: Option<mpsc::Sender<()>>,
        release: mpsc::Receiver<()>,
    }

    impl Write for BlockingOnceWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            if let Some(started) = self.started.take() {
                let _ = started.send(());
                let _ = self.release.recv();
            }
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn blocked_writer_never_blocks_the_control_owner() {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let writer = PtyInputWriter::spawn(
            Box::new(BlockingOnceWriter {
                started: Some(started_tx),
                release: release_rx,
            }),
            1,
            "test-pty-writer",
        )
        .unwrap();

        writer.try_write(vec![1]).unwrap();
        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        writer.try_write(vec![2]).unwrap();
        let started = Instant::now();
        assert_eq!(writer.try_write(vec![3]), Err(PtyInputSendError::Full));
        assert!(started.elapsed() < Duration::from_millis(100));
        let _ = release_tx.send(());
    }

    #[test]
    fn writer_failures_are_reported_to_the_control_owner() {
        struct FailedWriter;

        impl Write for FailedWriter {
            fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let writer =
            PtyInputWriter::spawn(Box::new(FailedWriter), 1, "test-failed-writer").unwrap();
        writer.try_write(vec![1]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Some(error) = writer.take_failure() {
                assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
                break;
            }
            assert!(Instant::now() < deadline, "writer failure was not reported");
            thread::yield_now();
        }
    }
}

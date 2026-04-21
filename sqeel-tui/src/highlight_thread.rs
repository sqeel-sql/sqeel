use sqeel_core::highlight::{HighlightSpan, Highlighter};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};

/// Runs tree-sitter highlighting on a dedicated thread.
///
/// Submit content with [`HighlightThread::submit`]; the thread always processes
/// the *latest* submitted value (older pending values are discarded). Poll for
/// completed spans with [`HighlightThread::try_recv`].
pub struct HighlightThread {
    pending: Arc<(Mutex<Option<String>>, Condvar)>,
    result_rx: mpsc::Receiver<Vec<HighlightSpan>>,
}

impl HighlightThread {
    pub fn spawn() -> anyhow::Result<Self> {
        let pending: Arc<(Mutex<Option<String>>, Condvar)> =
            Arc::new((Mutex::new(None), Condvar::new()));
        let (result_tx, result_rx) = mpsc::channel::<Vec<HighlightSpan>>();

        let pending_thread = Arc::clone(&pending);
        let mut highlighter = Highlighter::new()?;

        std::thread::Builder::new()
            .name("highlight".into())
            .spawn(move || {
                let (lock, cvar) = &*pending_thread;
                loop {
                    // Wait until there is content to process, then drain to latest.
                    let content = {
                        let mut guard = lock.lock().unwrap();
                        loop {
                            match guard.take() {
                                Some(c) => break c,
                                None => guard = cvar.wait(guard).unwrap(),
                            }
                        }
                    };

                    let spans = highlighter.highlight(&content);

                    if result_tx.send(spans).is_err() {
                        break; // main thread dropped the receiver
                    }
                }
            })?;

        Ok(Self { pending, result_rx })
    }

    /// Submit new content for highlighting. Replaces any not-yet-processed value.
    pub fn submit(&self, content: String) {
        let (lock, cvar) = &*self.pending;
        *lock.lock().unwrap() = Some(content);
        cvar.notify_one();
    }

    /// Returns the latest completed highlight result, if any.
    pub fn try_recv(&self) -> Option<Vec<HighlightSpan>> {
        // Drain to the latest result in case several arrived while we weren't polling.
        let mut latest = None;
        while let Ok(spans) = self.result_rx.try_recv() {
            latest = Some(spans);
        }
        latest
    }
}

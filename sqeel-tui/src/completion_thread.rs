use std::sync::{Arc, Condvar, Mutex};
use std::sync::mpsc;

/// Runs schema completion filtering on a dedicated thread.
///
/// Submit `(prefix, identifiers)` with [`CompletionThread::submit`]; the thread
/// always processes the latest submitted pair (older pending pairs are discarded).
/// Poll completed results with [`CompletionThread::try_recv`].
pub struct CompletionThread {
    pending: Arc<(Mutex<Option<(String, Vec<String>)>>, Condvar)>,
    result_rx: mpsc::Receiver<Vec<String>>,
}

impl CompletionThread {
    pub fn spawn() -> anyhow::Result<Self> {
        let pending: Arc<(Mutex<Option<(String, Vec<String>)>>, Condvar)> =
            Arc::new((Mutex::new(None), Condvar::new()));
        let (result_tx, result_rx) = mpsc::channel::<Vec<String>>();
        let pending_thread = Arc::clone(&pending);

        std::thread::Builder::new()
            .name("completion".into())
            .spawn(move || {
                let (lock, cvar) = &*pending_thread;
                loop {
                    let (prefix, identifiers) = {
                        let mut guard = lock.lock().unwrap();
                        loop {
                            match guard.take() {
                                Some(q) => break q,
                                None => guard = cvar.wait(guard).unwrap(),
                            }
                        }
                    };
                    let prefix_lower = prefix.to_lowercase();
                    let mut results: Vec<String> = identifiers
                        .into_iter()
                        .filter(|name| name.to_lowercase().starts_with(&prefix_lower))
                        .collect();
                    results.sort();
                    results.dedup();
                    if result_tx.send(results).is_err() {
                        break;
                    }
                }
            })?;

        Ok(Self { pending, result_rx })
    }

    /// Submit a new completion query. Replaces any not-yet-processed pair.
    pub fn submit(&self, prefix: String, identifiers: Vec<String>) {
        let (lock, cvar) = &*self.pending;
        *lock.lock().unwrap() = Some((prefix, identifiers));
        cvar.notify_one();
    }

    /// Returns the latest completed result, if any.
    pub fn try_recv(&self) -> Option<Vec<String>> {
        let mut latest = None;
        while let Ok(items) = self.result_rx.try_recv() {
            latest = Some(items);
        }
        latest
    }
}

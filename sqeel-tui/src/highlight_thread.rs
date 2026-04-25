use sqeel_core::highlight::{Dialect, HighlightSpan, Highlighter, ParseError};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};

/// A unit of work submitted to the highlight thread: the source slice
/// and the absolute row inside the full buffer where slice row 0 lives.
/// The thread parses the slice; the caller re-anchors the returned spans
/// back into buffer coordinates.
#[derive(Clone)]
pub struct HighlightRequest {
    pub source: Arc<String>,
    pub start_row: usize,
    pub row_count: usize,
    pub dialect: Dialect,
}

/// Result returned by the highlight thread: slice-local spans + the
/// window `(start_row, row_count)` that was submitted with the request,
/// so the caller can re-anchor the spans into buffer coordinates and
/// know which rows to clear.
#[derive(Clone)]
pub struct HighlightResult {
    pub spans: Vec<HighlightSpan>,
    pub start_row: usize,
    pub row_count: usize,
    /// Parse errors harvested from the same tree-sitter run. Surfaces
    /// inline diagnostic underlines as a fallback when the LSP is
    /// absent or not producing messages.
    pub parse_errors: Vec<ParseError>,
    /// Block ranges (multi-row tree-sitter nodes) collected from the
    /// same parse — drives `:foldsyntax`. Rows are relative to
    /// `start_row`; the host re-anchors them via offset before
    /// pushing to the editor.
    pub block_ranges: Vec<(usize, usize)>,
}

/// Runs tree-sitter highlighting on a dedicated thread.
///
/// Submit work with [`HighlightThread::submit`]; the thread always
/// processes the *latest* submitted value (older pending values are
/// discarded).  Poll for completed spans with [`HighlightThread::try_recv`].
pub struct HighlightThread {
    pending: Arc<(Mutex<Option<HighlightRequest>>, Condvar)>,
    result_rx: mpsc::Receiver<HighlightResult>,
}

impl HighlightThread {
    pub fn spawn() -> anyhow::Result<Self> {
        let pending: Arc<(Mutex<Option<HighlightRequest>>, Condvar)> =
            Arc::new((Mutex::new(None), Condvar::new()));
        let (result_tx, result_rx) = mpsc::channel::<HighlightResult>();

        let pending_thread = Arc::clone(&pending);
        let mut highlighter = Highlighter::new()?;

        std::thread::Builder::new()
            .name("highlight".into())
            .spawn(move || {
                let (lock, cvar) = &*pending_thread;
                loop {
                    let req = {
                        let mut guard = lock.lock().unwrap();
                        loop {
                            match guard.take() {
                                Some(r) => break r,
                                None => guard = cvar.wait(guard).unwrap(),
                            }
                        }
                    };

                    let spans = highlighter.highlight_shared(&req.source, req.dialect);
                    let parse_errors = highlighter.last_errors().to_vec();
                    let block_ranges = highlighter.block_ranges();

                    if result_tx
                        .send(HighlightResult {
                            spans,
                            start_row: req.start_row,
                            row_count: req.row_count,
                            parse_errors,
                            block_ranges,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })?;

        Ok(Self { pending, result_rx })
    }

    /// Submit new content for highlighting. Replaces any not-yet-processed value.
    pub fn submit(
        &self,
        source: Arc<String>,
        start_row: usize,
        row_count: usize,
        dialect: Dialect,
    ) {
        let (lock, cvar) = &*self.pending;
        *lock.lock().unwrap() = Some(HighlightRequest {
            source,
            start_row,
            row_count,
            dialect,
        });
        cvar.notify_one();
    }

    /// Returns the latest completed highlight result, if any.
    pub fn try_recv(&self) -> Option<HighlightResult> {
        let mut latest = None;
        while let Ok(r) = self.result_rx.try_recv() {
            latest = Some(r);
        }
        latest
    }
}

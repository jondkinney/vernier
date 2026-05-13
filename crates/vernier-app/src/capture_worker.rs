//! Background-thread screen-capture worker.
//!
//! On macOS `CGWindowListCreateImage` is a synchronous 30–60 ms call on
//! a 2× Retina display. Running it inline on the daemon's main loop —
//! the same loop that processes pointer-move events — means cursor
//! moves arriving while a capture is in flight have to wait, and live
//! measure mode visibly stalls every 100 ms.
//!
//! `CaptureWorker` moves that call off the hot path: a dedicated thread
//! loops at a fixed cadence, calling `platform.capture_screen_native`
//! and dropping each result into a single-slot `Mutex<Option<NativeFrame>>`.
//! The daemon's hot path calls [`CaptureWorker::try_latest_frame`],
//! which `take`s whatever the worker has ready — without ever blocking
//! on the capture itself. Cursor handling decouples from capture: the
//! cursor moves at AppKit's natural rate, edge detection runs against
//! the freshest frame the worker has produced.
//!
//! Single-slot semantics ("latest wins"): if the daemon doesn't drain
//! the slot before the worker captures again, the older frame is
//! discarded. We never want a backlog of stale frames — for edge
//! detection the freshest one is always the best one.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use vernier_platform::{MonitorId, NativeFrame, Platform};

/// Owned handle to a background capture loop. Drop or call
/// [`Self::stop`] to terminate the thread; the worker holds an
/// `Arc<dyn Platform>` so it can outlive the call site that started
/// it. Move-only by design — there's exactly one slot per worker.
pub(crate) struct CaptureWorker {
    slot: Arc<Mutex<Option<NativeFrame>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl CaptureWorker {
    /// Spawn the worker. `interval` is the minimum delay between
    /// capture attempts; an individual capture may run longer than
    /// the interval (on macOS it routinely does), in which case the
    /// next capture starts immediately.
    pub(crate) fn start(
        platform: Arc<dyn Platform>,
        monitor: MonitorId,
        interval: Duration,
    ) -> Self {
        let slot = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let slot_w = Arc::clone(&slot);
        let stop_w = Arc::clone(&stop);
        let handle = thread::Builder::new()
            .name("vernier-capture".into())
            .spawn(move || run(platform, monitor, slot_w, stop_w, interval))
            .expect("vernier-capture thread spawn");
        Self {
            slot,
            stop,
            handle: Some(handle),
        }
    }

    /// Move the latest captured frame out of the slot, if one is
    /// available. Non-blocking — returns `None` instantly when the
    /// worker hasn't produced a new frame since the last call. The
    /// daemon's hot path keeps using its previously-held frame for
    /// edge detection in that case.
    pub(crate) fn try_latest_frame(&self) -> Option<NativeFrame> {
        // `lock()` here can in principle wait for the worker's write
        // to release, but the write is just a `*g = Some(frame)`
        // assignment plus the drop of the previous Option — drop on
        // a Vec<u8> of ~10 MB is microseconds. Never an issue at the
        // ~10 Hz the worker produces. If contention ever shows up,
        // switch to `try_lock` and treat busy as "no new frame".
        self.slot.lock().ok().and_then(|mut g| g.take())
    }

    /// Signal the worker to stop and wait for it to finish. Join
    /// blocks up to one capture cycle (the worker checks the stop
    /// flag once per loop iteration). Called explicitly on
    /// measure-mode exit so the daemon doesn't leave a stray thread
    /// running between sessions.
    pub(crate) fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for CaptureWorker {
    fn drop(&mut self) {
        // Last-resort cleanup when the worker is dropped without
        // explicit `stop()` (panics, early returns). Don't join from
        // Drop — if the worker is mid-capture we'd block the dropping
        // thread for up to 60 ms. The thread will see the stop flag
        // on its next iteration and exit on its own; the OS reclaims
        // its resources when it returns.
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn run(
    platform: Arc<dyn Platform>,
    monitor: MonitorId,
    slot: Arc<Mutex<Option<NativeFrame>>>,
    stop: Arc<AtomicBool>,
    interval: Duration,
) {
    log::debug!("capture worker: started, interval={interval:?}");
    while !stop.load(Ordering::Relaxed) {
        let started = Instant::now();
        match platform.capture_screen_native(monitor) {
            Ok(frame) => {
                if let Ok(mut g) = slot.lock() {
                    // Latest wins: if the daemon hasn't drained the
                    // previous frame yet, drop it. The freshest frame
                    // is always the most useful for edge detection.
                    *g = Some(frame);
                }
            }
            Err(e) => {
                // Common in the wild: monitor disconnected, display
                // sleeping, capture permission revoked. Don't spam
                // info-level logs — the daemon will recover on its
                // own once capture becomes available again.
                log::debug!("capture worker: capture failed: {e}");
            }
        }
        let elapsed = started.elapsed();
        if elapsed < interval {
            // Sleep just the remainder so a slow capture eats into
            // the next iteration's budget rather than stretching the
            // total cycle out. Edge detection wants fresh frames.
            thread::sleep(interval - elapsed);
        }
    }
    log::debug!("capture worker: stopped");
}

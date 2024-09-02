use crate::RingBuffer;
use futures_io::AsyncWrite;
use std::cmp::min;
use std::intrinsics::copy_nonoverlapping;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::Poll::{Pending, Ready};
use std::task::{Context, Poll};

/// Write access to a nonblocking ring buffer with fixed capacity.
///
/// If there is no space in the buffer to write to, the current task is parked
/// and notified once space becomes available.
pub struct Writer(Arc<Mutex<RingBuffer>>);

impl Writer {
    #[doc(hidden)]
    pub(crate) fn new(buf: Arc<Mutex<RingBuffer>>) -> Self {
        Self(buf)
    }

    /// Returns true if the writer has been closed, and will therefore no longer
    ///  accept writes.
    pub fn is_closed(&self) -> bool {
        self.0.lock().unwrap().did_shutdown
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.0.lock().unwrap().wake();
    }
}

impl AsyncWrite for Writer {
    /// Write data to the RingBuffer.
    ///
    /// This only returns `Ok(Ready(0))` if either `buf.len() == 0`, `poll_close` has been called,
    /// or if the corresponding `Reader` has been dropped and no more data will be read to free up
    /// space for new data.
    ///
    /// # Errors
    /// This never emits an error.
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<io::Result<usize>> {
        let mut rb = self.0.lock().unwrap();

        if buf.is_empty() || rb.did_shutdown {
            return Ready(Ok(0));
        }

        let capacity = rb.data.capacity();
        let start = rb.data.as_mut_slice().as_mut_ptr();
        let end = unsafe { start.add(capacity) }; // end itself is 1 byte outside the buffer

        if rb.amount == capacity {
            if Arc::strong_count(&self.0) == 1 {
                return Ready(Ok(0));
            } else {
                rb.park(cx.waker());
                return Pending;
            }
        }

        let buf_ptr = buf.as_ptr();
        let write_total = min(buf.len(), capacity - rb.amount);

        if (unsafe { rb.write_ptr().add(write_total) } as *const u8) < end {
            // non-wrapping case
            unsafe { copy_nonoverlapping(buf_ptr, rb.write_ptr(), write_total) };

            rb.amount += write_total;
        } else {
            // wrapping case
            let distance_we = crate::offset_from(end, rb.write_ptr()) as usize;
            let remaining: usize = write_total - distance_we;

            unsafe { copy_nonoverlapping(buf_ptr, rb.write_ptr(), distance_we) };
            unsafe { copy_nonoverlapping(buf_ptr.add(distance_we), start, remaining) };

            rb.amount += write_total;
        }

        debug_assert!(unsafe { rb.read_ptr() } >= start);
        debug_assert!(unsafe { rb.read_ptr() } < end);
        debug_assert!(rb.amount <= capacity);

        rb.wake();
        Ready(Ok(write_total))
    }

    /// # Errors
    /// This never emits an error.
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context) -> Poll<io::Result<()>> {
        Ready(Ok(()))
    }

    /// Once closing is complete, the corresponding reader will always return `Ok(Ready(0))` on
    /// `poll_read` once all remaining buffered data has been read.
    ///
    /// # Errors
    /// This never emits an error.
    fn poll_close(self: Pin<&mut Self>, _: &mut Context) -> Poll<io::Result<()>> {
        let mut rb = self.0.lock().unwrap();

        if !rb.did_shutdown {
            rb.wake(); // only unpark on first call, makes this method idempotent
        }
        rb.did_shutdown = true;

        Ready(Ok(()))
    }
}

#[cfg(feature = "tokio")]
impl tokio::io::AsyncWrite for Writer {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        AsyncWrite::poll_write(self, cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_flush(self, cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        AsyncWrite::poll_close(self, cx)
    }
}

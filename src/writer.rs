use crate::RingBuffer;
use futures_io::AsyncWrite;
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
        let mut lock = self.0.lock().unwrap();
        lock.did_shutdown = true;
        lock.wake_reader();
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

        let read_ptr = unsafe { rb.read_ptr() };
        let write_ptr = unsafe { rb.write_ptr() };

        if read_ptr == write_ptr && rb.count == rb.size {
            return if Arc::strong_count(&self.0) == 1 {
                Ready(Ok(0))
            } else {
                rb.park_writer(cx.waker());
                Pending
            };
        }

        let start = rb.data.as_ptr() as usize;
        let end = start + rb.size; // end itself is 1 byte outside the buffer

        let buf_ptr = buf.as_ptr();
        let mut write_total = buf.len();

        if (unsafe { rb.write_ptr().add(write_total) } as usize) < end {
            // non-wrapping case
            unsafe { copy_nonoverlapping(buf_ptr, rb.write_ptr(), write_total) };

            rb.write_offset += write_total;
        } else {
            // wrapping case
            let distance_we = end - (write_ptr as usize);
            unsafe { copy_nonoverlapping(buf_ptr, rb.write_ptr(), distance_we) };

            let mut remaining: usize = write_total - distance_we;
            if (start + remaining) > read_ptr as usize {
                remaining -= (start + remaining) - read_ptr as usize;
            }

            unsafe { copy_nonoverlapping(buf_ptr, start as *mut u8, remaining) };

            write_total = distance_we + remaining;
            rb.write_offset = remaining;
        }

        rb.count += write_total;

        debug_assert!(unsafe { rb.write_ptr() } as usize >= start);
        debug_assert!((unsafe { rb.write_ptr() } as usize) < end);

        rb.wake_reader();
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
            rb.wake_reader(); // only unpark on first call, makes this method idempotent
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

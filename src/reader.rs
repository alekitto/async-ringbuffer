use crate::RingBuffer;
use futures_io::AsyncRead;
use std::cmp::min;
use std::intrinsics::copy_nonoverlapping;
use std::io;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::task::Poll::{Pending, Ready};
use std::task::{Context, Poll};

/// Read access to a nonblocking ring buffer with fixed capacity.
///
/// If there is no data in the buffer to read from, the current task is parked
/// and notified once data becomes available.
pub struct Reader(Arc<Mutex<RingBuffer>>);

impl Reader {
    #[doc(hidden)]
    pub(crate) fn new(buf: Arc<Mutex<RingBuffer>>) -> Self {
        Self(buf)
    }

    /// Returns true if the writer side of the ringbuffer has been closed.
    /// Reads will continue to produce data as long as there are still unread
    /// bytes in the ringbuffer.
    pub fn is_closed(&self) -> bool {
        self.0.lock().unwrap().did_shutdown
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.0.lock().unwrap().wake();
    }
}

impl AsyncRead for Reader {
    /// Read data from the RingBuffer.
    ///
    /// This only returns `Ok(Ready(0))` if either `buf.len() == 0`, `poll_close`
    /// was called on the corresponding `Writer` and all buffered data has been read, or if the
    /// corresponding `Writer` has been dropped.
    ///
    /// # Errors
    /// This never emits an error.
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let mut rb = self.0.lock().unwrap();

        if buf.is_empty() {
            return Ready(Ok(0));
        }

        let capacity = rb.data.capacity();
        let start = rb.data.as_mut_slice().as_mut_ptr();
        let end = unsafe { start.add(capacity) }; // end itself is 1 byte outside the buffer

        if rb.amount == 0 {
            if Arc::strong_count(&self.0) == 1 || rb.did_shutdown {
                return Ready(Ok(0));
            } else {
                rb.park(cx.waker());
                return Pending;
            }
        }

        let buf_ptr = buf.as_mut_ptr();
        let read_total = min(buf.len(), rb.amount);

        if (unsafe { rb.read_ptr().add(read_total) } as *const u8) < end {
            // non-wrapping case
            unsafe { copy_nonoverlapping(rb.read_ptr(), buf_ptr, read_total) };

            rb.read_offset.fetch_add(read_total, Ordering::SeqCst);
            rb.amount -= read_total;
        } else {
            // wrapping case
            let distance_re = crate::offset_from(end, unsafe { rb.read_ptr() }) as usize;
            let remaining: usize = read_total - distance_re;

            unsafe { copy_nonoverlapping(rb.read_ptr(), buf_ptr, distance_re) };
            unsafe { copy_nonoverlapping(start, buf_ptr.add(distance_re), remaining) };

            rb.read_offset.store(remaining, Ordering::SeqCst);
            rb.amount -= read_total;
        }

        debug_assert!(unsafe { rb.read_ptr() } >= start);
        debug_assert!(unsafe { rb.read_ptr() } < end);
        debug_assert!(rb.amount <= capacity);

        rb.wake();
        Ready(Ok(read_total))
    }
}

#[cfg(feature = "tokio")]
impl tokio::io::AsyncRead for Reader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // We can't trust the inner type to not peak at the bytes,
        // so we must defensively initialize the buffer.
        let slice = buf.initialize_unfilled();
        let n = futures::ready!(AsyncRead::poll_read(self, cx, slice))?;
        buf.advance(n);
        Ready(Ok(()))
    }
}

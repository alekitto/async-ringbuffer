//! An asynchronous, fixed-capacity single-reader single-writer ring buffer that notifies the reader onces data becomes available, and notifies the writer once new space for data becomes available. This is done via the AsyncRead and AsyncWrite traits.

#![deny(missing_docs)]

extern crate futures_io;

#[cfg(test)]
extern crate futures;

use std::sync::{Arc, Mutex};
use std::task::Waker;

mod duplex;
mod reader;
mod writer;

pub use duplex::Duplex;
pub use reader::Reader;
pub use writer::Writer;

/// Creates a new RingBuffer with the given capacity, and returns a handle for
/// writing and a handle for reading.
///
/// # Panics
/// Panics if capacity is `0` or greater than `isize::max_value()`.
pub fn ring_buffer(capacity: usize) -> (Writer, Reader) {
    if capacity == 0 || capacity > (isize::MAX as usize) {
        panic!("Invalid ring buffer capacity.");
    }

    let data = vec![0; capacity].into_boxed_slice();
    let rb = Arc::new(Mutex::new(RingBuffer {
        data,
        size: capacity,
        count: 0,
        read_offset: 0,
        write_offset: 0,
        read_waker: None,
        write_waker: None,
        did_shutdown: false,
    }));

    (Writer::new(Arc::clone(&rb)), Reader::new(rb))
}

struct RingBuffer {
    data: Box<[u8]>,
    size: usize,
    count: usize,
    // reading resumes from this position, this always points into the buffer
    read_offset: usize,
    write_offset: usize,
    read_waker: Option<Waker>,
    write_waker: Option<Waker>,
    did_shutdown: bool,
}

fn offset_from<T>(x: *const T, other: *const T) -> isize
where
    T: Sized,
{
    let size = size_of::<T>();
    assert_ne!(size, 0);
    let diff = (x as isize).wrapping_sub(other as isize);
    diff / size as isize
}

impl RingBuffer {
    fn park_reader(&mut self, waker: &Waker) {
        self.read_waker = Some(waker.clone());
    }
    fn park_writer(&mut self, waker: &Waker) {
        self.write_waker = Some(waker.clone());
    }

    fn wake_reader(&mut self) {
        if let Some(w) = self.read_waker.take() {
            w.wake()
        }
    }

    fn wake_writer(&mut self) {
        if let Some(w) = self.write_waker.take() {
            w.wake()
        }
    }

    unsafe fn read_ptr(&self) -> *mut u8 {
        self.data.as_ref().as_ptr().add(self.read_offset) as *mut u8
    }

    unsafe fn write_ptr(&self) -> *mut u8 {
        self.data.as_ref().as_ptr().add(self.write_offset) as *mut u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use futures::future::join;
    use futures::io::{AsyncReadExt, AsyncWriteExt};
    use std::time::Duration;

    #[test]
    fn it_works() {
        let (mut writer, mut reader) = ring_buffer(8);
        let data: Vec<u8> = (0..255).collect();
        let write_all = async {
            writer.write_all(&data).await.unwrap();
            writer.close().await.unwrap();
        };

        let mut out: Vec<u8> = Vec::with_capacity(256);
        let read_all = reader.read_to_end(&mut out);

        block_on(async { join(write_all, read_all).await.1.unwrap() });

        for (i, byte) in out.iter().enumerate() {
            assert_eq!(*byte, i as u8);
        }
    }

    #[test]
    #[should_panic]
    /// Calling `ring_buffer` with capacity 0 panics
    fn panic_on_capacity_0() {
        let _ = ring_buffer(0);
    }

    #[test]
    #[should_panic]
    /// Calling `ring_buffer` with capacity (isize::max_value() as usize) + 1 panics
    fn panic_on_capacity_too_large() {
        let _ = ring_buffer((isize::max_value() as usize) + 1);
    }

    #[test]
    fn close() {
        let (mut writer, mut reader) = ring_buffer(8);
        block_on(async {
            writer.write_all(&[1, 2, 3, 4, 5]).await.unwrap();
            assert!(!writer.is_closed());
            assert!(!reader.is_closed());

            writer.close().await.unwrap();

            assert!(writer.is_closed());
            assert!(reader.is_closed());

            let r = writer.write_all(&[6, 7, 8]).await;
            assert!(r.is_err());

            let mut buf = [0; 8];
            let n = reader.read(&mut buf).await.unwrap();
            assert_eq!(n, 5);

            let n = reader.read(&mut buf).await.unwrap();
            assert_eq!(n, 0);
        });
    }

    #[tokio::test]
    async fn do_not_overwrite() {
        let (mut writer, mut reader) = ring_buffer(8);
        tokio::task::spawn(async move {
            for _ in 0..255 {
                writer.write_all(&[0]).await.unwrap();
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });

        let mut buf = vec![];
        reader.read_to_end(&mut buf).await.unwrap();
        assert_eq!(255, buf.len());
    }
}

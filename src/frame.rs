use std::sync::{Arc, Mutex};
use tokio::io;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
#[derive(PartialEq, Debug)]
pub enum Status {
    Completed,
    Processing,
}

/// Recycles frame data buffers so reading a frame doesn't need a fresh (multi-MB) allocation.
#[derive(Default)]
pub struct FramePool {
    buffers: Mutex<Vec<Box<[u8]>>>,
}

impl FramePool {
    pub fn take(self: &Arc<Self>, data_len: usize, num: u64) -> Frame {
        let data = self
            .buffers
            .lock()
            .unwrap()
            .pop()
            .filter(|buffer| buffer.len() == data_len)
            .unwrap_or_else(|| vec![0; data_len].into_boxed_slice());
        Frame {
            num,
            data_len,
            data,
            pool: Some(self.clone()),
        }
    }
}

#[derive(Clone)]
pub struct Frame {
    pub num: u64,
    pub data_len: usize,
    pub data: Box<[u8]>,
    pool: Option<Arc<FramePool>>,
}

impl Drop for Frame {
    fn drop(&mut self) {
        // Hand the buffer back to its pool once the last reference to the frame is gone.
        if let Some(pool) = self.pool.take() {
            pool.buffers
                .lock()
                .unwrap()
                .push(std::mem::take(&mut self.data));
        }
    }
}

impl<'a> Frame {
    pub fn new(data_len: usize, num: u64) -> Self {
        Frame {
            num,
            data_len,
            data: vec![0; data_len].into_boxed_slice(),
            pool: None,
        }
    }

    pub fn data(&self) -> &[u8] {
        self.data.as_ref()
    }

    pub async fn read(
        &mut self,
        frame_num: u64,
        reader: &mut (impl AsyncBufReadExt + Unpin),
    ) -> io::Result<Status> {
        self.num = frame_num;
        if reader.fill_buf().await?.is_empty() {
            return Ok(Status::Completed);
        }
        let mut header = [0u8; 6];
        reader.read_exact(&mut header).await?;
        assert_eq!(&header, b"FRAME\x0A");
        reader.read_exact(self.data.as_mut()).await?;
        Ok(Status::Processing)
    }

    pub async fn write(&self, writer: &mut (impl AsyncWriteExt + Unpin)) -> io::Result<()> {
        writer.write_all(b"FRAME\x0A").await?;
        writer.write_all(self.data()).await?;
        Ok(())
    }
}

unsafe impl Send for Frame {}
unsafe impl Sync for Frame {}

#[cfg(test)]
mod tests {
    use crate::frame::FramePool;
    use std::sync::Arc;

    #[test]
    fn pool_reuses_dropped_buffer() {
        let pool = Arc::new(FramePool::default());
        let frame = pool.take(10, 0);
        let ptr = frame.data.as_ptr();
        drop(frame);
        let frame = pool.take(10, 1);
        assert_eq!(ptr, frame.data.as_ptr());
        assert_eq!(1, frame.num);
    }
}

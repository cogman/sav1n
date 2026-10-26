use crate::frame::{Frame, FramePool, Status};
use crate::video_header::VideoHeader;
use std::alloc::Layout;
use std::collections::VecDeque;
use std::pin::pin;
use std::sync::{Arc, Mutex};
use tokio::io;
use tokio::io::AsyncBufReadExt;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

/// Internal entry that holds a frame together with its semaphore permit.
/// The permit is kept so that when the entry is dropped (e.g., on pop),
/// the permit is released back to the semaphore, allowing new frames to be added.
struct BufferEntry {
    frame: Arc<Frame>,
    _permit: OwnedSemaphorePermit,
}

pub struct FrameBufferData {
    finished: bool,
    frame_number: u64,
    frames: VecDeque<BufferEntry>,
}

pub struct FrameBuffer {
    video_header: VideoHeader,
    // Never held across an await, so a std mutex is cheaper than an async lock.
    data: Mutex<FrameBufferData>,
    frame_layout: Layout,
    frame_size: usize,
    frames_len: usize,
    wait_for_frame: Notify,
    // Semaphore that limits the number of stored frames to `frames_len`.
    semaphore: Arc<Semaphore>,
    pool: Arc<FramePool>,
}

impl<'a> FrameBuffer {
    pub fn new(frames: usize, video_header: VideoHeader) -> Self {
        let frame_header = Layout::new::<Frame>();
        let frame_size = video_header.calc_frame_size();
        let frame_layout = frame_header
            .extend(Layout::array::<u8>(frame_size).expect("Overflow"))
            .expect("Overflow")
            .0;

        let buffer = FrameBufferData {
            finished: false,
            frame_number: 0,
            frames: VecDeque::new(),
        };

        let frame_buffer = FrameBuffer {
            data: Mutex::new(buffer),
            video_header,
            frames_len: frames,
            frame_size,
            frame_layout,
            wait_for_frame: Notify::new(),
            semaphore: Arc::new(Semaphore::new(frames)),
            pool: Arc::new(FramePool::default()),
        };

        frame_buffer
    }

    pub async fn add_frame(&self, frame: Frame) {
        // Acquire a permit before taking the lock. This will block if the buffer is full.
        let permit = self.semaphore.clone().acquire_owned().await.unwrap();
        let mut w = self.data.lock().unwrap();
        w.frame_number = frame.num + 1;
        w.frames.push_back(BufferEntry {
            frame: Arc::new(frame),
            _permit: permit,
        });
        drop(w);
        self.wait_for_frame.notify_waiters();
    }

    /// Returns the requested frame, waiting for it to be read in if necessary.
    /// Returns `None` once the input has ended and `frame_num` is past the last frame.
    pub async fn get_frame(&self, frame_num: u64) -> Option<Arc<Frame>> {
        loop {
            let mut notified = pin!(self.wait_for_frame.notified());
            {
                let d = self.data.lock().unwrap();
                if frame_num < d.frame_number {
                    let oldest = d.frames.front().map_or(d.frame_number, |e| e.frame.num);
                    assert!(
                        frame_num >= oldest,
                        "Frame fell out of buffer! asked for {}, earliest frame is {}",
                        frame_num,
                        oldest
                    );
                    return Some(d.frames[(frame_num - oldest) as usize].frame.clone());
                }
                if d.finished {
                    return None;
                }
                // Register as a waiter while still holding the lock so a frame added
                // between dropping the lock and awaiting can't be missed.
                notified.as_mut().enable();
            }
            notified.await;
        }
    }

    pub async fn read_in_frame(
        &self,
        reader: &mut (impl AsyncBufReadExt + Unpin),
    ) -> io::Result<Status> {
        // Acquire a permit before taking the lock; this will block if the buffer is full.
        let permit = self.semaphore.clone().acquire_owned().await.unwrap();
        let mut frame = self.pool.take(self.frame_size, 0);
        let status = frame.read(frame.num, reader).await?;
        let mut w = self.data.lock().unwrap();
        if status == Status::Processing {
            frame.num = w.frame_number;
            w.frames.push_back(BufferEntry {
                frame: Arc::new(frame),
                _permit: permit,
            });
            w.frame_number += 1;
        } else {
            w.finished = true;
        }
        drop(w);
        self.wait_for_frame.notify_waiters();
        Ok(status)
    }

    /// Removes the oldest frame from the buffer.
    pub async fn pop(&self) -> Option<Arc<Frame>> {
        // Removing the entry drops its OwnedSemaphorePermit, releasing a slot.
        self.data
            .lock()
            .unwrap()
            .frames
            .pop_front()
            .map(|entry| entry.frame)
    }

    pub async fn size(&self) -> usize {
        self.data.lock().unwrap().frames.len()
    }
}

unsafe impl Send for FrameBuffer {}
unsafe impl Sync for FrameBuffer {}

#[cfg(test)]
mod tests {
    use crate::frame::{Frame, Status};
    use crate::frame_buffer::FrameBuffer;
    use crate::video_header::{ColorSpaceType, VideoHeader};
    use std::io::{stdout, Cursor};
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use tokio::time::Duration;

    #[test]
    fn new_test() {
        FrameBuffer::new(10, VideoHeader::new());
    }

    #[tokio::test]
    async fn add_test() {
        let mut buffer = FrameBuffer::new(10, VideoHeader::new());
        buffer.add_frame(Frame::new(10, 0)).await;
        assert_eq!(buffer.size().await, 1)
    }

    #[tokio::test]
    async fn add_full_test() {
        let mut buffer = FrameBuffer::new(2, VideoHeader::new());
        let frame = Frame::new(10, 1);
        buffer.add_frame(frame.clone()).await;
        assert_eq!(buffer.size().await, 1);
        buffer.add_frame(frame).await;
        assert_eq!(buffer.size().await, 2);
    }

    #[tokio::test]
    async fn pop_test() {
        let mut buffer = FrameBuffer::new(1, VideoHeader::new());
        let mut stored_frame = Frame::new(10, 1);
        stored_frame.data[9] = 42;
        buffer.add_frame(stored_frame).await;
        let frame_option = buffer.pop().await;
        assert!(frame_option.is_some());
        let frame = frame_option.unwrap();
        assert_eq!(frame.num, 1);
        assert_eq!(buffer.size().await, 0);
        assert_eq!(42, frame.data()[9])
    }

    #[tokio::test]
    async fn double_pop_test() {
        let mut buffer = FrameBuffer::new(2, VideoHeader::new());
        buffer.add_frame(Frame::new(10, 1)).await;
        buffer.add_frame(Frame::new(10, 2)).await;
        let frame_option = buffer.pop().await;
        assert!(frame_option.is_some());
        let frame = frame_option.unwrap();
        assert_eq!(frame.num, 1);
        assert_eq!(frame.data_len, 10);
        assert_eq!(buffer.size().await, 1);

        let frame_option = buffer.pop().await;
        assert!(frame_option.is_some());
        let frame = frame_option.unwrap();
        assert_eq!(frame.num, 2);
        assert_eq!(frame.data_len, 10);
        assert_eq!(buffer.size().await, 0);
    }

    #[tokio::test]
    async fn specific_frame_test_present() {
        let mut buffer = FrameBuffer::new(2, VideoHeader::new());
        buffer.add_frame(Frame::new(10, 0)).await;
        let frame = buffer.get_frame(0).await.unwrap();
        assert_eq!(frame.num, 0)
    }

    #[tokio::test]
    async fn specific_frame_test_not_present() {
        let buffer = Arc::new(FrameBuffer::new(2, VideoHeader::new()));

        let clone1 = buffer.clone();
        let frame = tokio::spawn(async move {
            return clone1.get_frame(0).await.unwrap();
        });
        let clone2 = buffer.clone();
        let _ = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = clone2.add_frame(Frame::new(10, 0)).await;
        });
        assert_eq!(frame.await.unwrap().num, 0)
    }

    #[tokio::test]
    async fn specific_frame_test_take_end() {
        let mut buffer = FrameBuffer::new(2, VideoHeader::new());
        buffer.add_frame(Frame::new(10, 0)).await;
        buffer.add_frame(Frame::new(10, 1)).await;
        let frame = buffer.get_frame(1).await.unwrap();
        assert_eq!(frame.num, 1)
    }

    #[tokio::test]
    async fn read_two_frames() {
        let mut buffer = FrameBuffer::new(
            2,
            VideoHeader {
                width: 2,
                height: 1,
                rate: "123".to_string(),
                interlace: None,
                aspect_ratio: None,
                color_space_type: Some(ColorSpaceType::C440),
                as_bytes: vec![],
            },
        );
        let mut data: Vec<u8> = vec![];
        data.extend(b"FRAME\x0AwhatFRAME\x0Alove");
        let mut test_file = Cursor::new(data);
        let status0 = buffer.read_in_frame(&mut test_file).await.unwrap();
        let status1 = buffer.read_in_frame(&mut test_file).await.unwrap();
        assert_eq!(status0, Status::Processing);
        assert_eq!(status1, Status::Processing);
        let frame0 = buffer.get_frame(0).await.unwrap();
        let frame1 = buffer.get_frame(1).await.unwrap();
        assert_eq!(frame0.data(), b"what");
        assert_eq!(frame1.data(), b"love");
    }

    fn tiny_header() -> VideoHeader {
        VideoHeader {
            width: 2,
            height: 1,
            rate: "123".to_string(),
            interlace: None,
            aspect_ratio: None,
            color_space_type: Some(ColorSpaceType::C440),
            as_bytes: vec![],
        }
    }

    #[tokio::test]
    async fn get_frame_past_end_returns_none() {
        let buffer = FrameBuffer::new(2, tiny_header());
        let mut test_file = Cursor::new(b"FRAME\x0Awhat".to_vec());
        assert_eq!(
            buffer.read_in_frame(&mut test_file).await.unwrap(),
            Status::Processing
        );
        assert_eq!(
            buffer.read_in_frame(&mut test_file).await.unwrap(),
            Status::Completed
        );
        assert_eq!(buffer.get_frame(0).await.unwrap().data(), b"what");
        assert!(buffer.get_frame(1).await.is_none());
    }

    #[tokio::test]
    async fn get_frame_waits_for_future_frame() {
        let buffer = Arc::new(FrameBuffer::new(2, tiny_header()));
        let mut test_file = Cursor::new(b"FRAME\x0AwhatFRAME\x0Alove".to_vec());
        buffer.read_in_frame(&mut test_file).await.unwrap();

        let reader = buffer.clone();
        let frame = tokio::spawn(async move { reader.get_frame(1).await.unwrap() });
        tokio::time::sleep(Duration::from_millis(50)).await;
        buffer.read_in_frame(&mut test_file).await.unwrap();

        let frame = tokio::time::timeout(Duration::from_millis(100), frame)
            .await
            .expect("waiter should be woken without polling")
            .unwrap();
        assert_eq!(frame.data(), b"love");
    }

    #[tokio::test]
    async fn full_buffer_unblocks_on_pop() {
        let buffer = Arc::new(FrameBuffer::new(1, tiny_header()));
        let writer = buffer.clone();
        let read = tokio::spawn(async move {
            let mut test_file = Cursor::new(b"FRAME\x0AwhatFRAME\x0Alove".to_vec());
            writer.read_in_frame(&mut test_file).await.unwrap();
            writer.read_in_frame(&mut test_file).await.unwrap();
        });

        assert_eq!(buffer.get_frame(0).await.unwrap().data(), b"what");
        assert_eq!(buffer.pop().await.unwrap().num, 0);
        tokio::time::timeout(Duration::from_secs(1), read)
            .await
            .expect("reader deadlocked on a full buffer")
            .unwrap();
        assert_eq!(buffer.get_frame(1).await.unwrap().data(), b"love");
    }
}

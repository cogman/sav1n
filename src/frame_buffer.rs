use crate::frame::{Frame, Status};
use crate::video_header::VideoHeader;
use std::alloc::Layout;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio::io;
use tokio::io::AsyncBufReadExt;
use tokio::sync::{Notify, OwnedSemaphorePermit, RwLock, Semaphore};
use tokio::time::timeout;

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
    data: RwLock<FrameBufferData>,
    frame_layout: Layout,
    frame_size: usize,
    frames_len: usize,
    wait_for_frame: Notify,
    // Semaphore that limits the number of stored frames to `frames_len`.
    semaphore: Arc<Semaphore>,
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
            data: RwLock::new(buffer),
            video_header,
            frames_len: frames,
            frame_size,
            frame_layout,
            wait_for_frame: Notify::new(),
            semaphore: Arc::new(Semaphore::new(frames)),
        };

        frame_buffer
    }

    pub async fn add_frame(&self, frame: Frame) {
        // Acquire a permit before inserting. This will block if the buffer is full.
        let permit = self.semaphore.clone().acquire_owned().await.unwrap();
        let entry = BufferEntry {
            frame: Arc::new(frame),
            _permit: permit,
        };
        self.data.write().await.frames.push_front(entry);
        self.wait_for_frame.notify_waiters();
    }

    pub async fn get_frame(&self, frame_num: u64) -> Option<Arc<Frame>> {
        loop {
            let d = self.data.read().await;
            let front = d.frames.front();
            let finished = d.finished;
            if finished {
                return None;
            }
            if let Some(entry) = front {
                if entry.frame.num <= frame_num {
                    let index = (frame_num - entry.frame.num) as usize;
                    return Some(d.frames[index].frame.clone());
                }
            }
            let f = timeout(Duration::from_millis(500), self.wait_for_frame.notified());
            drop(d);
            f.await.ok();
        }
    }

    pub async fn read_in_frame(
        &self,
        reader: &mut (impl AsyncBufReadExt + Unpin),
    ) -> io::Result<Status> {
        let mut frame = Frame::new(self.frame_size, 0);
        let status = frame.read(frame.num, reader).await?;
        if status == Status::Processing {
            let mut w = self.data.write().await;
            // Acquire a permit for the new frame; this will block if the buffer is full.
            let permit = self.semaphore.clone().acquire_owned().await.unwrap();
            frame.num = w.frame_number;
            let entry = BufferEntry {
                frame: Arc::new(frame),
                _permit: permit,
            };
            w.frames.push_back(entry);
            w.frame_number += 1;
        }
        self.wait_for_frame.notify_waiters();
        Ok(status)
    }

    pub async fn pop(&self) -> Option<Arc<Frame>> {
        // Removing the entry drops its OwnedSemaphorePermit, releasing a slot.
        self.data
            .write()
            .await
            .frames
            .pop_back()
            .map(|entry| entry.frame)
    }

    pub async fn size(&self) -> usize {
        self.data.read().await.frames.len()
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
}

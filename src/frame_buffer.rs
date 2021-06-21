pub mod frame_buffer {
    use tokio::sync::{Semaphore, Notify};
    use crate::frame::frame::{Frame, Status};
    use std::alloc::{Layout};
    use crate::video_header::VideoHeader;
    use tokio::io::AsyncBufReadExt;
    use tokio::io;
    use std::panic::RefUnwindSafe;
    use std::cell::UnsafeCell;

    pub struct FrameBufferData {
        frame_number: u64,
        head: usize,
        tail: usize,
        size: usize,
        frames: *mut u8,
    }

    pub struct FrameBuffer {
        video_header: VideoHeader,
        data: UnsafeCell<FrameBufferData>,
        frame_layout: Layout,
        available: Semaphore,
        std_mutex: Semaphore,
        frame_size: usize,
        frames_len: usize,
        wait_for_frame: Notify
    }

    impl<'a> FrameBuffer {
        pub fn new(frames: usize, video_header: VideoHeader) -> Self {
            let frame_header = Layout::new::<Frame>();
            let frame_size = video_header.calc_frame_size();
            let frame_layout = frame_header
                .extend(Layout::array::<u8>(frame_size)
                    .expect("Overflow"))
                .expect("Overflow").0;
            let buffer_frames: *mut u8;
            unsafe {
                buffer_frames = std::alloc::alloc(Layout::array::<u8>(frame_layout.size() * frames)
                    .expect("Overflow"));
            }

            let buffer = FrameBufferData {
                frame_number: 0,
                head: 0,
                tail: 0,
                size: 0,
                frames: buffer_frames,
            };

            let frame_buffer = FrameBuffer {
                data: UnsafeCell::new(buffer),
                video_header,
                frames_len: frames,
                frame_size,
                frame_layout,
                available: Semaphore::new(frames),
                std_mutex: Semaphore::new(1),
                wait_for_frame: Notify::new()
            };

            for i in 0..frames {
                let frame_addr = frame_buffer.frame_addr(i);
                let frame: *mut Frame = frame_addr.cast();
                unsafe { (*frame).droppable = false; }
                unsafe { (*frame).data = frame_addr.add(frame_header.size()); }
            }

            return frame_buffer;
        }

        fn frame_addr(&self, i: usize) -> *mut u8 {
            let frame_addr = unsafe {
                (*self.ptr()).frames.add(self.frame_layout.size() * i)
            };
            return frame_addr;
        }

        fn frame_index(&self, i: usize) -> &'a mut Frame {
            let frame_ptr: *mut Frame = self.frame_addr(i).cast();
            return unsafe { frame_ptr.as_mut() }
                .expect("Should have casted correctly");
        }

        pub async fn add_frame(&self, frame: &Frame) {
            let stored_frame = self.reserve_frame().await;
            stored_frame.clone_from(frame);
            self.wait_for_frame.notify_waiters();
            self.std_mutex.add_permits(1);
        }

        pub async fn get_frame(&self, frame_num: u64) -> &'a Frame {
            let mut permit = self.std_mutex.acquire().await.unwrap();
            let mut next_frame = unsafe { (*self.ptr()).frame_number };
            let mut earliest_frame =
                if self.frames_len as u64 > frame_num { 0 } else { next_frame - (self.frames_len - 1) as u64 };

            if frame_num < earliest_frame {
                panic!("Frame {} is already out of the buffer!", frame_num);
            }
            while frame_num >= next_frame {
                permit.forget();
                self.std_mutex.add_permits(1);
                self.wait_for_frame.notified().await;
                permit = self.std_mutex.acquire().await.unwrap();
                unsafe { next_frame = (*self.ptr()).frame_number; }
            }

            earliest_frame =
                if self.frames_len as u64 > frame_num { 0 } else { next_frame - (self.frames_len - 1) as u64 };
            return self.frame_index((frame_num - earliest_frame) as usize);
        }

        async fn reserve_frame(&self) -> &'a mut Frame {
            self.available.acquire().await
                .expect("Failed to acquire available mutex")
                .forget();
            let _ = self.std_mutex.acquire().await.unwrap().forget();
            unsafe {
                let mut ptr = (self.ptr());
                let tail = (*ptr).tail;
                (*ptr).tail += 1;
                if (*ptr).tail >= self.frames_len {
                    (*ptr).tail = 0;
                }
                (*ptr).size += 1;
                let frame = self.frame_index(tail);
                frame.num = (*ptr).frame_number;
                frame.data_len = self.frame_size;
                (*ptr).frame_number += 1;
                return frame;
            }
        }

        pub async fn read_in_frame(&self, reader: &mut (impl AsyncBufReadExt + Unpin)) -> io::Result<Status> {
            let frame = self.reserve_frame().await;
            let status = frame.read(frame.data_len, frame.num, reader).await?;
            self.wait_for_frame.notify_waiters();
            self.std_mutex.add_permits(1);
            return Ok(status);
        }

        pub async fn pop(&self) -> Option<&'a Frame> {
            let _ = self.std_mutex.acquire().await.unwrap();
            unsafe {
                let this = self.ptr();
                return if (*this).size == 0 {
                    None
                } else {
                    let head = (*this).head;
                    (*this).size -= 1;
                    (*this).head += 1;
                    if (*this).head >= self.frames_len {
                        (*this).head = 0;
                    }
                    self.available.add_permits(1);
                    let frame = self.frame_index(head);
                    Some(frame)
                };
            }
        }

        pub async fn size(&self) -> usize {
            let _ = self.std_mutex.acquire().await.unwrap();
            return unsafe { (*self.ptr()).size };
        }

        fn ptr(&self) -> *mut FrameBufferData {
            return self.data.get()
        }
    }

    impl Drop for FrameBuffer {
        fn drop(&mut self) {
            unsafe {
                let data = self.data.get();
                std::alloc::dealloc((*data).frames.cast(),
                                    Layout::array::<u8>(self.frame_layout.size() * self.frames_len)
                    .expect("Overflow"));
            }
        }
    }

    unsafe impl Send for FrameBuffer { }
    unsafe impl Sync for FrameBuffer { }
    impl RefUnwindSafe for FrameBuffer { }
}

#[cfg(test)]
mod tests {
    use crate::frame_buffer::frame_buffer::FrameBuffer;
    use crate::frame::frame::Frame;
    use crate::video_header::{VideoHeader, ColorSpaceType};
    use std::sync::Arc;
    use tokio::time::Duration;
    use std::io::Cursor;

    #[test]
    fn new_test() {
        FrameBuffer::new(10, VideoHeader::new());
    }

    #[tokio::test]
    async fn add_test() {
        let buffer = FrameBuffer::new(10, VideoHeader::new());
        buffer.add_frame(&Frame::new(10, 0)).await;
        assert_eq!(buffer.size().await, 1)
    }

    #[tokio::test]
    async fn add_full_test() {
        let buffer = FrameBuffer::new(2, VideoHeader::new());
        let frame = Frame::new(10, 1);
        buffer.add_frame(&frame).await;
        assert_eq!(buffer.size().await, 1);
        buffer.add_frame(&frame).await;
        assert_eq!(buffer.size().await, 2);
    }

    #[tokio::test]
    async fn pop_test() {
        let buffer = FrameBuffer::new(1, VideoHeader::new());
        let mut stored_frame = Frame::new(10, 1);
        stored_frame.data_mut()[9] = 42;
        buffer.add_frame(&stored_frame).await;
        let frame_option = buffer.pop().await;
        assert!(frame_option.is_some());
        let frame = frame_option.unwrap();
        assert_eq!(frame.num, 1);
        assert_eq!(frame.data_len, 10);
        assert!(!frame.droppable);
        assert_eq!(buffer.size().await, 0);
        assert_eq!(42, frame.data()[9])
    }

    #[tokio::test]
    async fn double_pop_test() {
        let buffer = FrameBuffer::new(2, VideoHeader::new());
        buffer.add_frame(&Frame::new(10, 1)).await;
        buffer.add_frame(&Frame::new(10, 2)).await;
        let frame_option = buffer.pop().await;
        assert!(frame_option.is_some());
        let frame = frame_option.unwrap();
        assert_eq!(frame.num, 1);
        assert_eq!(frame.data_len, 10);
        assert!(!frame.droppable);
        assert_eq!(buffer.size().await, 1);

        let frame_option = buffer.pop().await;
        assert!(frame_option.is_some());
        let frame = frame_option.unwrap();
        assert_eq!(frame.num, 2);
        assert_eq!(frame.data_len, 10);
        assert!(!frame.droppable);
        assert_eq!(buffer.size().await, 0);
    }

    #[tokio::test]
    async fn specific_frame_test_present() {
        let buffer = FrameBuffer::new(2, VideoHeader::new());
        buffer.add_frame(&Frame::new(10, 0)).await;
        let frame = buffer.get_frame(0).await;
        assert_eq!(frame.num, 0)
    }

    #[tokio::test]
    async fn specific_frame_test_not_present() {
        let buffer = Arc::new(FrameBuffer::new(2, VideoHeader::new()));

        let clone1 = buffer.clone();
        let frame = tokio::spawn(async move {
            return clone1.get_frame(0).await;
        });
        let clone2 = buffer.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = clone2.add_frame(&Frame::new(10, 0)).await;
        });
        assert_eq!(frame.await.unwrap().num, 0)
    }

    #[tokio::test]
    async fn specific_frame_test_take_end() {
        let buffer = FrameBuffer::new(2, VideoHeader::new());
        buffer.add_frame(&Frame::new(10, 0)).await;
        buffer.add_frame(&Frame::new(10, 1)).await;
        let frame = buffer.get_frame(1).await;
        assert_eq!(frame.num, 1)
    }

    #[tokio::test]
    async fn read_two_frames() {
        let buffer = FrameBuffer::new(2, VideoHeader{
            width: 2,
            height: 1,
            rate: "123".to_string(),
            interlace: None,
            aspect_ratio: None,
            color_space_type: Some(ColorSpaceType::C440),
            as_bytes: vec![]
        });
        let mut data: Vec<u8> = vec!();
        data.extend(b"FRAME\x0AwhatFRAME\x0Alove");
        let mut test_file = Cursor::new(data);
        buffer.read_in_frame(&mut test_file).await.unwrap();
        buffer.read_in_frame(&mut test_file).await.unwrap();
        let frame0 = buffer.get_frame(0).await;
        let frame1 = buffer.get_frame(1).await;
        assert_eq!(frame0.data(), b"what");
        assert_eq!(frame1.data(), b"love");
    }
}
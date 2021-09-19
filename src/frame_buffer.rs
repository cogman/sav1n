
use crate::frame::{Frame, Status};
use crate::video_header::VideoHeader;
use std::alloc::Layout;
use std::cell::UnsafeCell;
use std::panic::RefUnwindSafe;
use tokio::io;
use tokio::io::AsyncBufReadExt;
use tokio::sync::{Notify, Semaphore};

pub struct FrameBufferData {
    finished: bool,
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
    wait_for_frame: Notify,
}

impl<'a> FrameBuffer {
    pub fn new(frames: usize, video_header: VideoHeader) -> Self {
        let frame_header = Layout::new::<Frame>();
        let frame_size = video_header.calc_frame_size();
        let frame_layout = frame_header
            .extend(Layout::array::<u8>(frame_size).expect("Overflow"))
            .expect("Overflow")
            .0;
        let buffer_frames: *mut u8;
        unsafe {
            buffer_frames = std::alloc::alloc(
                Layout::array::<u8>(frame_layout.size() * frames).expect("Overflow"),
            );
        }

        let buffer = FrameBufferData {
            finished: false,
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
            wait_for_frame: Notify::new(),
        };

        for i in 0..frames {
            let frame_addr = frame_buffer.frame_addr(i);
            let frame: *mut Frame = frame_addr.cast();
            unsafe {
                (*frame).droppable = false;
            }
            unsafe {
                (*frame).data = frame_addr.add(frame_header.size());
            }
        }

        frame_buffer
    }

    fn frame_addr(&self, i: usize) -> *mut u8 {
        unsafe { (*self.ptr()).frames.add(self.frame_layout.size() * i) }
    }

    fn frame_index(&self, i: usize) -> &'a mut Frame {
        let frame_ptr: *mut Frame = self.frame_addr(i).cast();
        return unsafe { frame_ptr.as_mut() }.expect("Should have casted correctly");
    }

    pub async fn add_frame(&self, frame: &Frame) {
        let stored_frame = self.reserve_frame().await;
        stored_frame.clone_from(frame);
        self.wait_for_frame.notify_waiters();
        self.std_mutex.add_permits(1);
    }

    pub async fn get_frame(&self, frame_num: u64) -> Option<&'a Frame> {
        let mut permit = self.std_mutex.acquire().await.unwrap();

        while frame_num >= unsafe { (*self.ptr()).frame_number } {
            if unsafe { (*self.ptr()).finished } {
                return None;
            }
            permit.forget();
            self.std_mutex.add_permits(1);
            self.wait_for_frame.notified().await;
            permit = self.std_mutex.acquire().await.unwrap();
        }
        let mut tail = unsafe { (*self.ptr()).tail };
        if tail == 0 {
            tail = self.frames_len - 1;
        } else {
            tail -= 1;
        }
        let tail_num = self.frame_index(tail).num;
        let head = unsafe { (*self.ptr()).head };
        let head_num = self.frame_index(head).num;
        if frame_num < head_num {
            panic!(
                "Frame fell out of buffer! asked for {}, earliest frame is {}",
                frame_num, head_num
            )
        }
        let offset = tail_num - frame_num;
        let index: u64;
        if offset > tail as u64 {
            let leftover = offset - tail as u64;
            index = self.frames_len as u64 - leftover;
        } else {
            index = tail as u64 - offset;
        }
        let frame = self.frame_index(index as usize);
        if frame.num != frame_num {
            panic!(
                "Returned the wrong frame! Returned {} asked for {}",
                frame.num, frame_num
            );
        }
        Some(frame)
    }

    async fn reserve_frame(&self) -> &'a mut Frame {
        self.available
            .acquire()
            .await
            .expect("Failed to acquire available mutex")
            .forget();
        let _ = self.std_mutex.acquire().await.unwrap().forget();
        unsafe {
            let mut ptr = self.ptr();
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
            frame
        }
    }

    pub async fn read_in_frame(
        &self,
        reader: &mut (impl AsyncBufReadExt + Unpin),
    ) -> io::Result<Status> {
        let frame = self.reserve_frame().await; // Holds onto the std_mutex
        let status = frame.read(frame.data_len, frame.num, reader).await?;
        if status == Status::Completed {
            // Rewind the tail
            unsafe {
                let mut ptr = self.ptr();
                (*ptr).finished = true;
                (*ptr).size -= 1;
                if (*ptr).tail > 0 {
                    (*ptr).tail -= 1;
                } else {
                    (*ptr).tail = self.frames_len - 1;
                }
                (*ptr).frame_number -= 1;
            }
        }
        self.wait_for_frame.notify_waiters();
        self.std_mutex.add_permits(1); // Released because reserve_frame forgets std_mutex
        Ok(status)
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
        unsafe { (*self.ptr()).size }
    }

    fn ptr(&self) -> *mut FrameBufferData {
        self.data.get()
    }
}

impl Drop for FrameBuffer {
    fn drop(&mut self) {
        unsafe {
            let data = self.data.get();
            std::alloc::dealloc(
                (*data).frames.cast(),
                Layout::array::<u8>(self.frame_layout.size() * self.frames_len).expect("Overflow"),
            );
        }
    }
}

unsafe impl Send for FrameBuffer {}
unsafe impl Sync for FrameBuffer {}
impl RefUnwindSafe for FrameBuffer {}

#[cfg(test)]
mod tests {
    use crate::frame::{Frame, Status};
    use crate::frame_buffer::FrameBuffer;
    use crate::video_header::{ColorSpaceType, VideoHeader};
    use std::io::Cursor;
    use std::sync::Arc;
    use tokio::time::Duration;

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
        let frame = buffer.get_frame(1).await.unwrap();
        assert_eq!(frame.num, 1)
    }

    #[tokio::test]
    async fn read_two_frames() {
        let buffer = FrameBuffer::new(
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

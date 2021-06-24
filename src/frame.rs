pub mod frame {
    use std::alloc::Layout;
    use tokio::io;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

    #[derive(PartialEq, Debug)]
    pub enum Status {
        Completed,
        Processing
    }

    pub struct Frame {
        pub droppable: bool,
        pub data_len: usize,
        pub num: u64,
        pub data: *mut u8,
    }

    impl<'a> Frame {
        pub fn new(data_len: usize, num: u64) -> Self {
            return Frame {
                droppable: true,
                data_len,
                num,
                data: unsafe { std::alloc::alloc(Layout::array::<u8>(data_len).expect("Length too long")) }
            }
        }

        pub fn data(&self) -> &'a [u8] {
            unsafe {  std::slice::from_raw_parts(self.data, self.data_len) }
        }

        pub fn data_mut(&mut self) -> &'a mut [u8] {
            unsafe {  std::slice::from_raw_parts_mut(self.data, self.data_len) }
        }

        pub async fn read(&mut self, frame_size: usize, frame_num: u64, reader: &mut (impl AsyncBufReadExt + Unpin)) -> io::Result<Status> {
            self.data_len = frame_size;
            self.num = frame_num;
            let mut header: Vec<u8> = Vec::with_capacity(6);
            reader.read_until(b'\x0A', &mut header).await?;
            if header.is_empty() {
                return Ok(Status::Completed)
            }
            assert_eq!(&header, b"FRAME\x0A");
            reader.read_exact(self.data_mut()).await?;
            return Ok(Status::Processing);
        }

        pub async fn write(&self, writer: &mut (impl AsyncWriteExt + Unpin)) -> io::Result<()> {
            writer.write_all(b"FRAME\x0A").await?;
            writer.write_all(self.data()).await?;
            return Ok(())
        }
    }

    impl Clone for Frame {
        fn clone(&self) -> Self {
            let data_ptr;
            unsafe {
                data_ptr = std::alloc::alloc(Layout::array::<u8>(self.data_len)
                    .expect("weird layout"));
                std::ptr::copy(self.data, data_ptr, self.data_len);
            }
            return Frame {
                num: self.num,
                data_len: self.data_len,
                droppable: true,
                data: data_ptr
            }
        }

        fn clone_from(&mut self, source: &Self) {
            self.num = source.num;
            self.data_len = source.data_len;
            unsafe {
                std::ptr::copy(source.data, self.data, source.data_len)
            }
        }
    }

    impl Drop for Frame {
        fn drop(&mut self) {
            if self.droppable {
                unsafe {
                    std::alloc::dealloc(self.data, Layout::array::<u8>(self.data_len)
                        .expect("layout"))
                }
            }
        }
    }

    unsafe impl Send for Frame { }
    unsafe impl Sync for Frame { }
}

#[cfg(test)]
mod tests {
    use crate::frame::frame::Frame;
    use std::io::Cursor;

    #[tokio::test]
    async fn test_read_frame() {
        let mut frame = Frame::new(12, 1);
        let mut data: Vec<u8> = vec!();
        data.extend(b"FRAME\x0Awhat is love");
        let mut test_file = Cursor::new(data);
        frame.read(12, 2, &mut test_file).await.unwrap();
        assert_eq!(frame.num, 2);
        assert_eq!(frame.data(), b"what is love")
    }

    #[tokio::test]
    async fn test_read_two_frames() {
        let mut frame = Frame::new(12, 1);
        let mut data: Vec<u8> = vec!();
        data.extend(b"FRAME\x0Awhat is loveFRAME\x0Ais what love");
        let mut test_file = Cursor::new(data);
        frame.read(12, 2, &mut test_file).await.unwrap();
        assert_eq!(frame.num, 2);
        assert_eq!(frame.data(), b"what is love");

        frame.read(12, 3, &mut test_file).await.unwrap();
        assert_eq!(frame.num, 3);
        assert_eq!(frame.data(), b"is what love");
    }

    #[tokio::test]
    async fn test_write() {
        let mut frame = Frame::new(12, 1);
        let mut data: Vec<u8> = vec!();
        let output = b"FRAME\x0Awhat is love";
        data.extend(output);
        let mut test_file = Cursor::new(data);
        frame.read(12, 2, &mut test_file).await.unwrap();

        let write: Vec<u8> = vec!();
        let mut out_file = Cursor::new(write);
        frame.write(&mut out_file).await.unwrap();
        assert_eq!(output, out_file.get_ref().as_slice())
    }
}
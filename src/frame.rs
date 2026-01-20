use tokio::io;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
#[derive(PartialEq, Debug)]
pub enum Status {
    Completed,
    Processing,
}

#[derive(Clone)]
pub struct Frame {
    pub num: u64,
    pub data_len: usize,
    pub data: Box<[u8]>,
}

impl<'a> Frame {
    pub fn new(data_len: usize, num: u64) -> Self {
        Frame {
            num,
            data_len,
            data: vec![0; data_len].into_boxed_slice(),
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
        let mut header: Vec<u8> = Vec::with_capacity(6);
        reader.read_until(b'\x0A', &mut header).await?;
        if header.is_empty() {
            return Ok(Status::Completed);
        }
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

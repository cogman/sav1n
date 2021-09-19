use crate::video_header::ColorSpaceType::C420p10;
use std::fmt::Debug;
use std::io::ErrorKind;
use std::num::ParseIntError;
use std::str::Utf8Error;
use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, Error};

#[derive(PartialEq, Debug, Copy, Clone)]
pub enum ColorSpaceType {
    C410,
    C411,
    C420p10,
    C422,
    C440,
    C444,
}

#[allow(dead_code)]
#[derive(Clone)]
pub struct VideoHeader {
    pub width: u32,
    pub height: u32,
    pub rate: String,
    pub interlace: Option<String>,
    pub aspect_ratio: Option<String>,
    pub color_space_type: Option<ColorSpaceType>,
    pub as_bytes: Vec<u8>,
}

impl VideoHeader {
    pub fn new() -> Self {
        VideoHeader {
            width: 240,
            height: 160,
            rate: "1234".to_string(),
            interlace: None,
            aspect_ratio: None,
            color_space_type: None,
            as_bytes: vec![],
        }
    }

    pub fn calc_frame_size(&self) -> usize {
        let ctype = self.color_space_type.as_ref().unwrap_or(&C420p10);
        let pixels: usize = (self.width * self.height) as usize;
        match ctype {
            ColorSpaceType::C410 => (pixels * 5) / 4, // 10 bits?
            ColorSpaceType::C411 => (pixels * 3) / 2, // 12 bits
            ColorSpaceType::C420p10 => pixels * 3,    // 10 bits
            ColorSpaceType::C422 => pixels * 2,       // 16 bits
            ColorSpaceType::C440 => pixels * 2,       // 16 bits?
            ColorSpaceType::C444 => pixels * 3,       // 24 bits
        }
    }

    pub async fn write(self, writer: &mut (impl AsyncWriteExt + Unpin)) -> io::Result<()> {
        return writer.write_all(self.as_bytes.as_slice()).await;
    }

    pub async fn read(reader: &mut (impl AsyncBufReadExt + Unpin)) -> io::Result<VideoHeader> {
        let mut header: Vec<u8> = Vec::with_capacity(128);
        reader.read_until(b'\x0A', &mut header).await?;
        header.pop(); // Remove the final 0x0A
        let sections = header.split(|byte| *byte == b'\x20');

        let mut width: io::Result<u32> = Err(Error::new(ErrorKind::InvalidData, "Missing Width"));
        let mut height: io::Result<u32> = Err(Error::new(ErrorKind::InvalidData, "Missing Height"));
        let mut rate: io::Result<String> = Err(Error::new(ErrorKind::InvalidData, "Missing Rate"));
        let mut interlace: Option<String> = None;
        let mut aspect_ratio: Option<String> = None;
        let mut color_space_type: Option<ColorSpaceType> = None;
        let mut first = true;

        for section in sections {
            if first {
                if section != b"YUV4MPEG2" {
                    println!("{}", header.len());
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        format!("Wrong magic word {}", std::str::from_utf8(section).unwrap()),
                    ));
                }
                first = false;
                continue;
            }
            let tail = std::str::from_utf8(&section[1..])
                .map_err(|err: Utf8Error| Error::new(ErrorKind::InvalidData, err))?;
            match section[0] {
                b'W' => {
                    width = tail
                        .parse()
                        .map_err(|err: ParseIntError| Error::new(ErrorKind::InvalidData, err))
                }
                b'H' => {
                    height = tail
                        .parse()
                        .map_err(|err: ParseIntError| Error::new(ErrorKind::InvalidData, err))
                }
                b'F' => rate = Ok(tail.to_string()),
                b'I' => interlace = Some(tail.to_string()),
                b'A' => aspect_ratio = Some(tail.to_string()),
                b'C' => match tail {
                    "410" => color_space_type = Some(ColorSpaceType::C410),
                    "411" => color_space_type = Some(ColorSpaceType::C411),
                    "420p10" => color_space_type = Some(ColorSpaceType::C420p10),
                    "422" => color_space_type = Some(ColorSpaceType::C422),
                    "440" => color_space_type = Some(ColorSpaceType::C440),
                    "444" => color_space_type = Some(ColorSpaceType::C444),
                    _ => {
                        return Err(Error::new(
                            ErrorKind::InvalidData,
                            format!("Unsupported colorspace '{}'", tail),
                        ))
                    }
                },
                b'X' => {} // This is a comment parameter, not supported,
                _ => {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        format!(
                            "Unknown parameter '{}'",
                            std::str::from_utf8(section).expect("Section should parse")
                        ),
                    ))
                }
            }
        }

        header.push(b'\x0A');
        Ok(VideoHeader {
            width: width?,
            height: height?,
            rate: rate?,
            interlace,
            aspect_ratio,
            color_space_type,
            as_bytes: header,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::video_header::{ColorSpaceType, VideoHeader};
    use std::io::Cursor;

    #[tokio::test]
    async fn test_read() {
        let mut vec: Vec<u8> = Vec::new();
        vec.extend(b"YUV4MPEG2 W384 H288 F25:1 Ip A0:0 C420p10\x0A");
        let header = VideoHeader::read(&mut Cursor::new(vec))
            .await
            .expect("should succeed");

        assert_eq!(384, header.width);
        assert_eq!(288, header.height);
        assert_eq!("25:1", header.rate);
        assert_eq!("p", header.interlace.unwrap());
        assert_eq!(ColorSpaceType::C420p10, header.color_space_type.unwrap());
    }

    #[tokio::test]
    async fn test_write() {
        let header_bytes = b"YUV4MPEG2 W384 H288 F25:1 Ip A0:0 C420p10\x0A";
        let mut vec: Vec<u8> = Vec::new();
        vec.extend(header_bytes);
        let header = VideoHeader::read(&mut Cursor::new(vec))
            .await
            .expect("should succeed");

        let output = vec![];
        let mut buff = Cursor::new(output);
        header.write(&mut buff).await.unwrap();
        assert_eq!(buff.get_ref(), header_bytes);
    }

    #[tokio::test]
    async fn test_frame_size() {
        let mut vec: Vec<u8> = Vec::new();
        vec.extend(b"YUV4MPEG2 W640 H480 F25:1 Ip A0:0\x0A");
        let header = VideoHeader::read(&mut Cursor::new(vec))
            .await
            .expect("should succeed");

        assert_eq!(921600, header.calc_frame_size())
    }
}

mod frame_buffer;
mod frame;
mod video_header;
mod aom_firstpass;

use crate::frame_buffer::frame_buffer::FrameBuffer;
use crate::frame::frame::Frame;
use std::alloc::Layout;
use crate::video_header::VideoHeader;
use tokio::process::Command;
use tokio::task;
use std::process::Stdio;
use std::convert::TryInto;
use tokio::io::{BufReader, AsyncBufReadExt, BufWriter, AsyncWriteExt};
use crate::frame::frame::Status::Processing;
use clap::{App, Arg};
use std::mem::size_of;
use crate::aom_firstpass::AomFirstpass;
use std::sync::Arc;
use tokio::sync::Semaphore;

#[tokio::main]
async fn main() {
    let options = App::new("sav1n")
        .version("0.0.1")
        .author("Thomas May")
        .arg(Arg::new("input")
            .short('i')
            .long("input")
            .about("Input file")
            .required(true)
            .multiple(false)
            .takes_value(true))
        .get_matches();

    if let Some(input) = options.value_of("input") {
        let mut vspipe = Command::new("vspipe").arg("--y4m").arg(input).arg("-")
            .stdout(Stdio::piped())
            .spawn().unwrap();

        let mut aom = Command::new("aomenc")
            .arg("--passes=2")
            .arg("--pass=1")
            .arg("--fpf=keyframe.log")
            .arg("--end-usage=q")
            .arg("--threads=32")
            .arg("-o")
            .arg("/dev/null").arg("-")
            .stdin(Stdio::piped())
            .spawn().unwrap();

        let mut vspipe_output = vspipe.stdout.take().unwrap();
        let mut aom_input = aom.stdin.take().unwrap();

        let mut vs_pipe_reader = BufReader::with_capacity(1024, vspipe_output);
        let mut writer = BufWriter::with_capacity(1024, aom_input);

        task::spawn(async move {
            let status = vspipe.wait().await
                .expect("child process encountered an error");
            let _ = aom.wait().await.expect("aom failed to start");
            println!("child status was: {}", status);
        });

        let analyzed_aom_frames = Arc::new(Semaphore::new(0));
        let header = VideoHeader::read(&mut vs_pipe_reader).await.unwrap();

        let buffer = Arc::new(FrameBuffer::new(33, header.clone()));
        header.clone().write(&mut writer).await;

        let mut status = buffer.read_in_frame(&mut vs_pipe_reader).await.unwrap();
        let popper_buf = buffer.clone();
        let delayed_aom = analyzed_aom_frames.clone();
        let delayed_popper = task::spawn(async move {
            delayed_aom.acquire_many(32).await.unwrap().forget();
            loop {
                delayed_aom.acquire().await.unwrap().forget();
                let frame = popper_buf.pop().await;
                if frame.is_none() {
                    break;
                }
            }
        });

        let writing_buf = buffer.clone();
        let writing = task::spawn(async move {
            let mut frame_num = 0;
            loop {
                let frame = writing_buf.get_frame(frame_num).await;
                if let Some(f) = frame {
                    f.write(&mut writer).await;
                    analyzed_aom_frames.add_permits(1)
                } else {
                    analyzed_aom_frames.add_permits(100);
                    break;
                }
                frame_num += 1;
            }
        });

        while status == Processing {
            status = buffer.read_in_frame(&mut vs_pipe_reader).await.unwrap();
        }
        writing.await;
        delayed_popper.await;
    }
}
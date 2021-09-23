mod aom_firstpass;
mod frame;
mod frame_buffer;
mod video_header;

use crate::frame::Status::Processing;
use crate::frame_buffer::FrameBuffer;
use crate::video_header::VideoHeader;
use clap::{App, Arg, ArgMatches};
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{BufReader, BufWriter, AsyncWriteExt, AsyncWrite, ErrorKind};
use tokio::process::{Command, Child};
use tokio::sync::Semaphore;
use tokio::task;
use std::collections::VecDeque;
use crate::aom_firstpass::aom::AomFirstpass;
use tokio::fs::File;
use std::time::Duration;
use tokio::time::sleep;
use std::net::Shutdown;

#[tokio::main]
async fn main() {
    let options = extract_options();

    if let Some(input) = options.value_of("input") {
        let mut vspipe = start_vspipe(input);
        let mut aom = start_aom_scene_detection();

        let vspipe_output = vspipe.stdout.take().unwrap();
        let mut aom_input = aom.stdin.take().unwrap();

        let mut vs_pipe_reader = BufReader::with_capacity(1024, vspipe_output);

        task::spawn(async move {
            let status = vspipe
                .wait()
                .await
                .expect("child process encountered an error");
            println!("child status was: {}", status);
        });

        let analyzed_aom_frames = Arc::new(Semaphore::new(0));
        let header = VideoHeader::read(&mut vs_pipe_reader).await.unwrap();

        let buffer = Arc::new(FrameBuffer::new(129, header.clone()));
        header.clone().write(&mut aom_input).await.unwrap();

        let mut status = buffer.read_in_frame(&mut vs_pipe_reader).await.unwrap();
        let popper_buf = buffer.clone();
        let delayed_aom = analyzed_aom_frames.clone();
        let delayed_popper = task::spawn(async move {
            delayed_aom.acquire_many(96).await.unwrap().forget();
            let mut frame_stats = VecDeque::new();
            let mut keyframe = BufReader::with_capacity(1024, File::open("/tmp/keyframe.log").await.unwrap());
            let mut current = AomFirstpass::read_aom_firstpass(&mut keyframe).await.unwrap();
            println!("frame stats: {} ", current.frame);
            let mut last = current;
            // Fill up the frame buffer
            while frame_stats.len() < 16 {
                frame_stats.push_back(AomFirstpass::read_aom_firstpass(&mut keyframe).await.unwrap());
            }
            loop {
                delayed_aom.acquire().await.unwrap().forget();
                let frame = popper_buf.pop().await;
                if frame.is_none() {
                    break;
                }
                if current.test_candidate_kf(&last, &frame_stats) {
                    println!("Keyframe!")
                }
                last = current;
                current = frame_stats.pop_front().unwrap();
                let stats = AomFirstpass::read_aom_firstpass(&mut keyframe).await;
                match stats {
                    Ok(stat) => frame_stats.push_back(stat),
                    Err(e) => {
                        if e.kind() == ErrorKind::UnexpectedEof {
                            println!("stats done!");
                            break;
                        }
                        else {
                            panic!("Exploded unexpectedly! {}", e)
                        }
                    }
                }
            }
        });

        let writing_buf = buffer.clone();
        let writing = task::spawn(async move {
            let mut frame_num = 0;
            loop {
                let frame = writing_buf.get_frame(frame_num).await;
                if let Some(f) = frame {
                    f.write(&mut aom_input).await.unwrap();
                    analyzed_aom_frames.add_permits(1)
                } else {
                    aom_input.flush().await;
                    aom_input.shutdown().await.unwrap();
                    drop(aom_input);
                    aom.wait().await.expect("AOM crashed");
                    analyzed_aom_frames.add_permits(100);
                    break;
                }
                frame_num += 1;
            }
        });

        while status == Processing {
            status = buffer.read_in_frame(&mut vs_pipe_reader).await.unwrap();
        }
        writing.await.unwrap();
        delayed_popper.await.unwrap();
    }
}

fn start_aom_scene_detection() -> Child {
    Command::new("aomenc")
        .arg("--passes=2")
        .arg("--pass=1")
        .arg("--fpf=/tmp/keyframe.log")
        .arg("--end-usage=q")
        .arg("--threads=32")
        .arg("-o")
        .arg("/dev/null")
        .arg("-")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap()
}

fn start_vspipe(input: &str) -> Child {
    Command::new("vspipe")
        .arg("--y4m")
        .arg(input)
        .arg("-")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap()
}

fn extract_options() -> ArgMatches {
    App::new("sav1n")
        .version("0.0.1")
        .author("Thomas May")
        .arg(
            Arg::new("input")
                .short('i')
                .long("input")
                .about("Input file")
                .required(true)
                .multiple_values(true)
                .takes_value(true),
        )
        .get_matches()
}

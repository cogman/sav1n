mod aom_firstpass;
mod frame;
mod frame_buffer;
mod video_header;

use crate::aom_firstpass::aom::AomFirstpass;
use crate::frame::Status::Processing;
use crate::frame_buffer::FrameBuffer;
use crate::video_header::VideoHeader;
use clap::{App, Arg, ArgMatches};
use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::Arc;
use tokio::fs::File;
use tokio::io::{AsyncWriteExt, BufReader, ErrorKind};
use tokio::process::{Child, Command, ChildStdin};
use tokio::sync::{Semaphore, watch, broadcast};
use tokio::task;
use tokio::task::JoinHandle;
use tokio::sync::broadcast::Sender;
use std::borrow::Borrow;
use std::ops::{BitAnd, BitXor, Not};

#[tokio::main]
async fn main() {
    let options = extract_options();

    if let Some(input) = options.value_of("input") {
        let mut vspipe = start_vspipe(input);

        let vspipe_output = vspipe.stdout.take().unwrap();

        let mut vs_pipe_reader = BufReader::with_capacity(1024, vspipe_output);

        let analyzed_aom_frames = Arc::new(Semaphore::new(0));
        let header = VideoHeader::read(&mut vs_pipe_reader).await.unwrap();

        let buffer = Arc::new(FrameBuffer::new(129, header.clone()));

        let delayed_aom = analyzed_aom_frames.clone();
        let (stats_tx, mut stats_rx) = broadcast::channel(129);

        let frame_stats_processor = stats_processor(header.clone(), delayed_aom, stats_tx);

        let scene_buffer = buffer.clone();
        let vpx_header = header.clone();
        let vpx_processing = task::spawn(async move {
            let mut scene = 0;
            let mut file = File::create(format!("{:06}.y4m", scene)).await.unwrap();
            vpx_header.clone().write(&mut file).await;
            while let Ok(stat) = stats_rx.recv().await {
                let frame = scene_buffer.pop().await;
                if stat.is_keyframe {
                    file.shutdown();
                    scene += 1;
                    file = File::create(format!("{:06}.y4m", scene)).await.unwrap();
                    vpx_header.clone().write(&mut file).await;
                }
                if let Some(frame_data) = frame {
                    frame_data.write(&mut file).await;
                }
                else {
                    break;
                }
            }
        });

        let aom_first_pass_scene = aom_firstpass_for_scene_detection(header, analyzed_aom_frames, buffer.clone());

        let mut status = buffer.read_in_frame(&mut vs_pipe_reader).await.unwrap();
        while status == Processing {
            status = buffer.read_in_frame(&mut vs_pipe_reader).await.unwrap();
        }
        vpx_processing.await;
        aom_first_pass_scene.await;
        frame_stats_processor.await.unwrap();
    }
}

fn stats_processor(video_header: VideoHeader, delayed_aom: Arc<Semaphore>, stats_tx: Sender<FrameStats>) -> JoinHandle<()> {
    let frame_stats_processor = task::spawn(async move {
        delayed_aom.acquire_many(96).await.unwrap().forget();
        let mut frame_stats = VecDeque::new();
        let mut keyframe =
            BufReader::with_capacity(1024, File::open("/tmp/keyframe.log").await.unwrap());
        let mut current = AomFirstpass::read_aom_firstpass(&mut keyframe)
            .await
            .unwrap();
        let mut last = current;
        // Fill up the frame buffer
        while frame_stats.len() < 16 {
            frame_stats.push_back(
                AomFirstpass::read_aom_firstpass(&mut keyframe)
                    .await
                    .unwrap(),
            );
        }
        let mut since_last_keyframe = 1;
        let num_mbs = mbs(video_header.width, video_header.height);
        loop {
            delayed_aom.acquire().await.unwrap().forget();
            if current.test_candidate_kf(&last, &frame_stats, since_last_keyframe, num_mbs) {
                stats_tx.send(FrameStats {
                    frame_num: current.frame as u64,
                    is_keyframe: true
                });
                since_last_keyframe = 0;
            } else {
                stats_tx.send(FrameStats {
                    frame_num: current.frame as u64,
                    is_keyframe: false
                });
            }
            since_last_keyframe += 1;
            last = current;
            current = frame_stats.pop_front().unwrap();
            let stats = AomFirstpass::read_aom_firstpass(&mut keyframe).await;
            match stats {
                Ok(stat) => frame_stats.push_back(stat),
                Err(e) => {
                    if e.kind() == ErrorKind::UnexpectedEof {
                        stats_tx.send(FrameStats {
                            frame_num: current.frame as u64,
                            is_keyframe: false
                        });
                        for stats in frame_stats {
                            stats_tx.send(FrameStats {
                                frame_num: stats.frame as u64,
                                is_keyframe: false
                            });
                        }
                        drop(stats_tx);
                        break;
                    } else {
                        panic!("Exploded unexpectedly! {}", e)
                    }
                }
            }
        }
    });
    frame_stats_processor
}

fn aom_firstpass_for_scene_detection(video_header: VideoHeader, analyzed_aom_frames: Arc<Semaphore>, writing_buf: Arc<FrameBuffer>) -> JoinHandle<()> {
    let mut aom = start_aom_scene_detection();
    let mut aom_input = aom.stdin.take().unwrap();
    task::spawn(async move {
        video_header.clone().write(&mut aom_input).await.unwrap();
        let mut frame_num = 0;
        loop {
            let frame = writing_buf.get_frame(frame_num).await;
            frame_num += 1;
            if let Some(f) = frame {
                f.write(&mut aom_input).await.unwrap();
                analyzed_aom_frames.add_permits(1)
            } else {
                aom_input.flush().await.unwrap();
                aom_input.shutdown().await.unwrap();
                // Drop input at this point to kill the pipe and cause AOM to flush
                drop(aom_input);
                aom.wait().await.expect("AOM crashed");
                // Allows for the stats to process to the end
                analyzed_aom_frames.add_permits(99999);
                break;
            }
        }
    })
}

fn start_aom_scene_detection() -> Child {
    Command::new("aomenc")
        .arg("--passes=2")
        .arg("--pass=1")
        .arg("--bit-depth=10")
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
const MI_SIZE_LOG2: u32 = 2;

fn mbs(width: u32, height: u32) -> u32 {
    let aligned_width = align_power_of_two(width as i32, 3);
    let aligned_height = align_power_of_two(height as i32, 3);
    let mi_cols = aligned_width >> MI_SIZE_LOG2;
    let mi_rows = aligned_height >> MI_SIZE_LOG2;

    let mb_cols = (mi_cols + 2) >> 2;
    let mb_rows = (mi_rows + 2) >> 2;
    return (mb_rows * mb_cols) as u32;
}

fn align_power_of_two(value: i32, n: i32) -> i32 {
    let x = value + ((1 << n) - 1);
    let y = ((1 << n) - 1).not();
    return x.bitand(y);
}

#[derive(Clone)]
struct FrameStats {
    frame_num: u64,
    is_keyframe: bool
}
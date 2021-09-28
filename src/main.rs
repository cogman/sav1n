mod aom_firstpass;
mod frame;
mod frame_buffer;
mod video_header;

use crate::aom_firstpass::aom::AomFirstpass;
use crate::frame::Status::Processing;
use crate::frame_buffer::FrameBuffer;
use crate::video_header::VideoHeader;
use clap::{App, Arg, ArgMatches};
use lazy_static::lazy_static;
use regex::Regex;
use std::borrow::Borrow;
use std::collections::VecDeque;
use std::convert::TryInto;
use std::ops::{BitAnd, BitXor, Not};
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use tokio::fs::remove_file;
use tokio::fs::File;
use tokio::io::{AsyncWriteExt, BufReader, ErrorKind};
use tokio::join;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::broadcast::{Receiver, Sender};
use tokio::sync::{broadcast, watch, Mutex, MutexGuard, Semaphore};
use tokio::task;
use tokio::task::JoinHandle;
use tokio::time::{sleep, Duration};

#[tokio::main]
async fn main() {
    let options = extract_options();

    if let Some(input) = options.value_of("input") {
        let i = String::from(input);
        let encoders = options.value_of_t_or_exit("encoders");
        let vpy: String = options.value_of_t_or_exit("vpy");

        let audio_processing = encode_audio(i);

        let mut vspipe = start_vspipe(input, vpy.as_str());

        let vspipe_output = vspipe.stdout.take().unwrap();

        let mut vs_pipe_reader = BufReader::with_capacity(1024, vspipe_output);

        let analyzed_aom_frames = Arc::new(Semaphore::new(0));
        let header = VideoHeader::read(&mut vs_pipe_reader).await.unwrap();

        let buffer = Arc::new(FrameBuffer::new(129, header.clone()));

        let delayed_aom = analyzed_aom_frames.clone();
        let (stats_tx, mut stats_rx) = broadcast::channel(129);

        let frame_stats_processor = stats_processor(header.clone(), delayed_aom, stats_tx);

        let active_encodes = Arc::new(Mutex::new(Vec::new()));
        let vpx_processing = vpx_process(
            stats_rx,
            buffer.clone(),
            header.clone(),
            active_encodes.clone(),
            encoders,
        );

        let aom_first_pass_scene =
            aom_firstpass_for_scene_detection(header, analyzed_aom_frames, buffer.clone());

        let mut status = buffer.read_in_frame(&mut vs_pipe_reader).await.unwrap();
        while status == Processing {
            status = buffer.read_in_frame(&mut vs_pipe_reader).await.unwrap();
        }
        drop(vs_pipe_reader);
        drop(vspipe);
        let scenes = vpx_processing.await.unwrap();
        aom_first_pass_scene.await.unwrap();
        frame_stats_processor.await.unwrap();
        audio_processing.await.unwrap().wait().await;
        let encode_list = active_encodes.lock().await;
        let mut encode_len = encode_list.len();
        drop(encode_list);
        while encode_len > 0 {
            sleep(Duration::from_millis(500)).await;
            encode_len = active_encodes.lock().await.len();
        }

        let mut concat_file = File::create("/tmp/concat.txt").await.unwrap();
        for scene in 0..=scenes {
            let concat_line = format!("file '/tmp/{:06}.ivf'\n", scene);
            concat_file.write_all(concat_line.as_bytes()).await;
        }
        concat_file.flush();
        concat_file.shutdown();
        drop(concat_file);

        Command::new("ffmpeg")
            .arg("-f")
            .arg("concat")
            .arg("-safe")
            .arg("0")
            .arg("-i")
            .arg("/tmp/concat.txt")
            .arg("-c")
            .arg("copy")
            .arg("/tmp/video.mkv")
            .spawn()
            .unwrap()
            .wait()
            .await;

        if Path::new("/tmp/timecodes.txt").exists() {
            Command::new("ffmpeg")
                .arg("-i")
                .arg("/tmp/video.mkv")
                .arg("-i")
                .arg("/tmp/audio.mkv")
                .arg("-c")
                .arg("copy")
                .arg("/tmp/audiovideo.mkv")
                .spawn()
                .unwrap()
                .wait()
                .await;

            Command::new("mkvmerge")
                .arg("--output")
                .arg("output.mkv")
                .arg("--timestamps")
                .arg("0:/tmp/timecodes.txt")
                .arg("/tmp/audiovideo.mkv")
                .spawn()
                .unwrap()
                .wait()
                .await;
        }
        else {
            Command::new("ffmpeg")
                .arg("-i")
                .arg("/tmp/video.mkv")
                .arg("-i")
                .arg("/tmp/audio.mkv")
                .arg("-c")
                .arg("copy")
                .arg("output.mkv")
                .spawn()
                .unwrap()
                .wait()
                .await;
        }
    }
}

fn encode_audio(i: String) -> JoinHandle<Child> {
    task::spawn(async move {
        Command::new("ffmpeg")
            .stderr(Stdio::null())
            .stdout(Stdio::null())
            .arg("-i")
            .arg(i)
            .arg("-vn")
            .arg("-c:a")
            .arg("libopus")
            .arg("-b:a")
            .arg("128k")
            .arg("-vbr")
            .arg("on")
            .arg("/tmp/audio.mkv")
            .spawn().unwrap()
    })
}

fn vpx_process(
    mut stats_rx: Receiver<FrameStats>,
    scene_buffer: Arc<FrameBuffer>,
    vpx_header: VideoHeader,
    active_encodes_vpx: Arc<Mutex<Vec<u32>>>,
    encoders: usize,
) -> JoinHandle<u32> {
    task::spawn(async move {
        let mut scene: u32 = 0;
        let mut file = File::create(format!("/tmp/{:06}.y4m", scene))
            .await
            .unwrap();
        vpx_header.clone().write(&mut file).await;
        while let Ok(stat) = stats_rx.recv().await {
            if stat.is_keyframe {
                file.flush().await;
                file.shutdown().await;
                drop(file);
                let encode_list = active_encodes_vpx.lock().await;
                let mut encode_len = encode_list.len();
                drop(encode_list);
                while encode_len > encoders {
                    sleep(Duration::from_millis(500)).await;
                    encode_len = active_encodes_vpx.lock().await.len();
                }
                let mut guard = active_encodes_vpx.lock().await;
                guard.push(scene);
                drop(guard);
                compress_scene(scene, active_encodes_vpx.clone());
                scene += 1;
                file = File::create(format!("/tmp/{:06}.y4m", scene))
                    .await
                    .unwrap();
                vpx_header.clone().write(&mut file).await;
            }
            let frame = scene_buffer.get_frame(stat.frame_num).await;
            if let Some(frame_data) = frame {
                assert_eq!(stat.frame_num, frame_data.num);
                frame_data.write(&mut file).await;
                scene_buffer.pop().await;
            } else {
                break;
            }
        }
        let mut guard = active_encodes_vpx.lock().await;
        guard.push(scene);
        drop(guard);
        compress_scene(scene, active_encodes_vpx.clone());
        scene
    })
}

fn compress_scene(scene_number: u32, encoding_scenes: Arc<Mutex<Vec<u32>>>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut first_pass = first_pass(scene_number).await;
        first_pass.wait().await.unwrap();
        let cq = vmaf_secant_search(10, 60, 20, 40, 0.95, scene_number).await;
        second_pass(scene_number, cq).await.wait().await.unwrap();
        cleanup(scene_number).await;
        let mut encodes = encoding_scenes.lock().await;
        let index = encodes.binary_search(&scene_number).unwrap();
        encodes.remove(index);
    })
}

async fn first_pass(scene_number: u32) -> Child {
    let scene_str = format!("/tmp/{:06}", scene_number);
    Command::new("vpxenc")
        .arg("--quiet")
        .arg("--passes=2")
        .arg("--pass=1")
        .arg("-b")
        .arg("10")
        .arg("--profile=2")
        .arg("--threads=1")
        .arg(format!("--fpf={}.log", scene_str))
        .arg("--end-usage=q")
        .arg("-o")
        .arg("/dev/null")
        .arg(format!("{}.y4m", scene_str))
        .spawn()
        .unwrap()
}

async fn second_pass(scene_number: u32, cq: u32) -> Child {
    let scene_str = format!("/tmp/{:06}", scene_number);
    Command::new("vpxenc")
        .arg(format!("--cq-level={}", cq))
        .arg("--quiet")
        .arg("--passes=2")
        .arg("--pass=2")
        .arg("--profile=2")
        .arg("--good")
        .arg("--cpu-used=0")
        .arg("--lag-in-frames=25")
        .arg("--kf-max-dist=250")
        .arg("--auto-alt-ref=1")
        .arg("--arnr-strength=2")
        .arg("--arnr-maxframes=7")
        .arg("--enable-tpl=1")
        .arg("--threads=1")
        .arg("-b")
        .arg("10")
        .arg(format!("--fpf={}.log", scene_str))
        .arg("--end-usage=q")
        .arg("--ivf")
        .arg("-o")
        .arg(format!("{}.ivf", scene_str))
        .arg(format!("{}.y4m", scene_str))
        .spawn()
        .unwrap()
}

async fn cleanup(scene_number: u32) {
    let scene_str = format!("/tmp/{:06}", scene_number);
    let remove_video = remove_file(format!("{}.y4m", scene_str));
    let remove_scene = remove_file(format!("{}.log", scene_str));
    join!(remove_video, remove_scene);
}

async fn vmaf_second_pass(scene_number: u32, cq: u32) -> f64 {
    let scene_str = format!("/tmp/{:06}", scene_number);
    let mut vpx = Command::new("vpxenc")
        .arg(format!("--cq-level={}", cq))
        .arg("--quiet")
        .arg("--passes=2")
        .arg("--pass=2")
        .arg("--profile=2")
        .arg("-b")
        .arg("10")
        .arg(format!("--fpf={}.log", scene_str))
        .arg("--end-usage=q")
        .arg("--ivf")
        .arg("--cpu-used=6")
        .arg("--threads=1")
        .arg("-o")
        .arg("-")
        .arg(format!("{}.y4m", scene_str))
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    let mut vpx_stdin: Stdio = vpx
        .stdout
        .take()
        .unwrap()
        .try_into()
        .expect("failed to convert to Stdio");

    let mut ffmpeg = Command::new("ffmpeg")
        .stdin(vpx_stdin)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .arg("-i")
        .arg("pipe:0")
        .arg("-i")
        .arg(format!("{}.y4m", scene_str))
        .arg("-threads")
        .arg("1")
        .arg("-lavfi")
        .arg("libvmaf=model_path=/usr/local/share/model/vmaf_v0.6.1.json")
        .arg("-f")
        .arg("null")
        .arg("-")
        .spawn()
        .unwrap();

    let (_, ffmpeg_output) = join!(vpx.wait(), ffmpeg.wait_with_output());

    lazy_static! {
        static ref VMAF_RE: Regex = Regex::new(r"VMAF score:\s+([\d|.]+)").unwrap();
    }
    let results = String::from_utf8(ffmpeg_output.unwrap().stderr).unwrap();
    let captures = VMAF_RE.captures(results.as_str()).unwrap();
    let capture = &captures[1];

    capture.parse().map(|n: f64| n / 100.0).unwrap()
}

async fn vmaf_secant_search(
    min: u32,
    max: u32,
    initial_guess_min: u32,
    initial_guess_max: u32,
    target: f64,
    scene_number: u32,
) -> u32 {
    let mut x1 = initial_guess_min;
    let mut x2 = initial_guess_max;
    let first_fx1 = task::spawn(async move { vmaf_second_pass(scene_number, x1).await });
    let first_fx2 = task::spawn(async move { vmaf_second_pass(scene_number, x2).await });
    let (mut fx1_result, mut fx2_result) = join!(first_fx1, first_fx2);
    let mut fx1 = fx1_result.unwrap() - target;
    let mut fx2 = fx2_result.unwrap() - target;
    let mut iterations = 0;
    while fx1.abs() > 0.005 && iterations < 10 {
        let mut next = (x1 as f64 - (fx1 * ((x1 as f64 - x2 as f64) / (fx1 - fx2)))).floor() as u32;
        println!(
            "x1: {}, x2: {}, fx1: {}, fx2: {}, next: {}  ",
            x1, x2, fx1, fx2, next
        );
        x2 = x1;
        fx2 = fx1;
        if next < min {
            if x1 == min {
                println!("Bailing out min");
                return min;
            } else {
                next = min
            }
        } else if next > max {
            if x1 == max {
                println!("Bailing out max");
                return max;
            } else {
                next = max
            }
        } else if next == x1 {
            println!("Bailing out, next check the same");
            break;
        }
        x1 = next;
        fx1 = vmaf_second_pass(scene_number, x1).await - target;

        println!("tried cq: {}, got {} ", next, fx1 + target);
        iterations += 1;
    }
    println!("final cq: {} final vmaf: {} ", x1, fx1 + target);
    return if fx1 > 0.0 { x1 } else { (x1 - 1).max(min) };
}

fn stats_processor(
    video_header: VideoHeader,
    delayed_aom: Arc<Semaphore>,
    stats_tx: Sender<FrameStats>,
) -> JoinHandle<()> {
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
                    is_keyframe: true,
                });
                since_last_keyframe = 0;
            } else {
                stats_tx.send(FrameStats {
                    frame_num: current.frame as u64,
                    is_keyframe: false,
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
                            is_keyframe: false,
                        });
                        for stats in frame_stats {
                            stats_tx.send(FrameStats {
                                frame_num: stats.frame as u64,
                                is_keyframe: false,
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

fn aom_firstpass_for_scene_detection(
    video_header: VideoHeader,
    analyzed_aom_frames: Arc<Semaphore>,
    writing_buf: Arc<FrameBuffer>,
) -> JoinHandle<()> {
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
        .arg("--threads=1")
        .arg("-o")
        .arg("/dev/null")
        .arg("-")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap()
}

fn start_vspipe(input: &str, vpy: &str) -> Child {
    Command::new("vspipe")
        .arg("-c")
        .arg("y4m")
        .arg("-t")
        .arg("/tmp/timecodes.txt")
        .arg("--arg")
        .arg(format!("file={}", input))
        .arg(vpy)
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
                .multiple_values(false)
                .takes_value(true),
        )
        .arg(
            Arg::new("vpy")
                .short('v')
                .long("vpy")
                .about("vapoursynth file")
                .required(true)
                .multiple_values(false)
                .takes_value(true),
        )
        .arg(
            Arg::new("encoders")
                .short('e')
                .long("encoders")
                .about("Number of encoders")
                .default_value("12")
                .multiple_values(false)
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
    is_keyframe: bool,
}

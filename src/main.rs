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
use serde_json::Value;

use std::collections::VecDeque;
use std::convert::TryInto;
use std::ops::{BitAnd, Not};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use glob::{glob};
use tokio::fs::{create_dir, remove_dir_all, remove_file};
use tokio::fs::File;
use tokio::io::{AsyncWriteExt, BufReader, ErrorKind};
use tokio::join;
use tokio::process::{Child, Command};
use tokio::sync::broadcast::{Receiver, Sender};
use tokio::sync::{broadcast, Mutex, Semaphore};
use tokio::task;
use tokio::task::{JoinHandle};

extern crate jemallocator;

#[global_allocator]
static ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;

#[tokio::main]
async fn main() {
    let options = extract_options();

    let targets: Vec<PathBuf> = options.get_many::<String>("input")
        .expect("At least one input is required")
        .flat_map(move |f| glob(f.as_str()).unwrap())
        .map(move |f| f.unwrap())
        .collect();

    println!("Encoding {} files", targets.len());

    let encoders = options.value_of_t_or_exit("encoders");
    let cpu_used = options.value_of_t_or_exit("cpu_used");
    let vpy: String = options.value_of_t_or_exit("vpy");
    let vmaf_target: f64 = options.value_of_t_or_exit::<f64>("vmaf_target") / 100.0;
    let active_encodes = Arc::new(Semaphore::new(encoders));

    let mut tasks = vec![];
    let can_do_next = Arc::new(Semaphore::new(0));
    for entry in targets {
        let len = tasks.len();

        let vpy = vpy.clone();
        let active_encodes = active_encodes.clone();
        let entry = entry.clone();
        let cdn = can_do_next.clone();

        tasks.push(tokio::spawn(async move {
            compress_file(cpu_used, vpy, vmaf_target, active_encodes, entry, cdn, len).await
        }));
        can_do_next.acquire().await.unwrap().forget()
    }

    for task in tasks {
        task.await.unwrap();
    }
}

async fn compress_file(cpu_used: u32, vpy: String, vmaf_target: f64, active_encoders: Arc<Semaphore>, input_path: PathBuf, can_do_next: Arc<Semaphore>, processed_file: usize) {
    let i: String = input_path.to_str().unwrap().to_string();
    println!("Encoding {}", i);
    let tmp_folder = format!("/tmp/{}", processed_file);
    create_dir(&tmp_folder).await.unwrap();

    let audio_processing = encode_audio(i.clone(), active_encoders.clone(), tmp_folder.clone());

    let mut vspipe = start_vspipe(i.clone().as_str(), vpy.as_str(), tmp_folder.clone());
    let vspipe_output = vspipe.stdout.take().unwrap();
    let mut vs_pipe_reader = BufReader::with_capacity(1024, vspipe_output);

    let analyzed_aom_frames = Arc::new(Semaphore::new(0));
    let header = VideoHeader::read(&mut vs_pipe_reader).await.unwrap();

    let buffer = Arc::new(FrameBuffer::new(129, header.clone()));

    let delayed_aom = analyzed_aom_frames.clone();
    let (stats_tx, stats_rx) = broadcast::channel(129);

    let frame_stats_processor = stats_processor(header.clone(), delayed_aom, stats_tx, tmp_folder.clone());

    let vpx_processing = vpx_process(
        stats_rx,
        buffer.clone(),
        header.clone(),
        active_encoders.clone(),
        vmaf_target,
        cpu_used,
        tmp_folder.clone(),
    );

    let aom_first_pass_scene =
        aom_firstpass_for_scene_detection(header, analyzed_aom_frames, buffer.clone(), tmp_folder.clone());

    let mut status = buffer.read_in_frame(&mut vs_pipe_reader).await.unwrap();
    while status == Processing {
        status = buffer.read_in_frame(&mut vs_pipe_reader).await.unwrap();
    }
    drop(vs_pipe_reader);
    drop(vspipe);
    can_do_next.add_permits(1);
    let scenes = vpx_processing.await.unwrap();
    aom_first_pass_scene.await.unwrap();
    frame_stats_processor.await.unwrap();
    audio_processing.await.unwrap();

    let mut concat_file = File::create(format!("{}/concat.txt", tmp_folder)).await.unwrap();
    for scene in 0..=scenes {
        let concat_line = format!("file '{}/{:06}.ivf'\n", tmp_folder, scene);
        concat_file.write_all(concat_line.as_bytes()).await.unwrap();
    }
    concat_file.flush().await.unwrap();
    concat_file.shutdown().await.unwrap();
    drop(concat_file);

    Command::new("ffmpeg")
        .arg("-y")
        .arg("-f")
        .arg("concat")
        .arg("-safe")
        .arg("0")
        .arg("-i")
        .arg(format!("{}/concat.txt", tmp_folder))
        .arg("-c")
        .arg("copy")
        .arg(format!("{}/video.mkv", tmp_folder))
        .spawn()
        .unwrap()
        .wait()
        .await
        .unwrap();
    let output_name = String::from(
        input_path
            .with_extension("new.mkv")
            .file_name()
            .unwrap()
            .to_str()
            .unwrap(),
    );
    if Path::new(format!("{}/timecodes.txt", tmp_folder).as_str()).exists() {
        Command::new("ffmpeg")
            .arg("-y")
            .arg("-i")
            .arg(format!("{}/video.mkv", tmp_folder))
            .arg("-i")
            .arg(format!("{}/audio.mkv", tmp_folder))
            .arg("-map")
            .arg("0:v")
            .arg("-map")
            .arg("1:a")
            .arg("-map")
            .arg("1:s?")
            .arg("-c")
            .arg("copy")
            .arg(format!("{}/audiovideo.mkv", tmp_folder))
            .spawn()
            .unwrap()
            .wait()
            .await
            .unwrap();

        Command::new("mkvmerge")
            .arg("--output")
            .arg(output_name)
            .arg("--timestamps")
            .arg(format!("0:{}/timecodes.txt", tmp_folder))
            .arg(format!("{}/audiovideo.mkv", tmp_folder))
            .spawn()
            .unwrap()
            .wait()
            .await
            .unwrap();
    } else {
        Command::new("ffmpeg")
            .arg("-y")
            .arg("-i")
            .arg(format!("{}/video.mkv", tmp_folder))
            .arg("-i")
            .arg(format!("{}/audio.mkv", tmp_folder))
            .arg("-map")
            .arg("0:v")
            .arg("-map")
            .arg("1:a")
            .arg("-map")
            .arg("1:s?")
            .arg("-c")
            .arg("copy")
            .arg(output_name)
            .spawn()
            .unwrap()
            .wait()
            .await
            .unwrap();
    }

    remove_dir_all(tmp_folder).await.unwrap();
}

fn encode_audio(i: String, permits: Arc<Semaphore>, tmp_folder: String) -> JoinHandle<()> {
    task::spawn(async move {
        permits.acquire().await.unwrap().forget();
        let probe_results = Command::new("ffprobe")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .arg("-hide_banner")
            .arg("-print_format")
            .arg("json")
            .arg("-show_streams")
            .arg(&i)
            .spawn()
            .unwrap()
            .wait_with_output()
            .await
            .unwrap();

        let probe_result: Value = serde_json::from_slice(&probe_results.stdout).unwrap();
        let streams = probe_result["streams"].as_array().unwrap();

        let mut audio_encode = Command::new("ffmpeg");
        let subtitles = if i.ends_with("mp4") { "srt" } else { "copy" };
        let mut next_section = audio_encode
            .stderr(Stdio::null())
            .stdout(Stdio::null())
            .arg("-y")
            .arg("-i")
            .arg(&i)
            .arg("-map")
            .arg("0")
            .arg("-vn")
            .arg("-c:a")
            .arg("libopus")
            .arg("-vbr")
            .arg("on");
        let mut channel_map = Vec::new();
        let mut audio_index = 0;
        for (i, stream) in streams.iter().enumerate() {
            if stream["codec_type"] == "audio" {
                let mut channel_layout = stream["channel_layout"].as_str().unwrap();
                if channel_layout.ends_with("(side)") {
                    let len = channel_layout.len();
                    channel_layout = &channel_layout[0..len - "(side)".len()];
                }
                channel_map.push(format!(
                    "[:{}]channelmap=channel_layout='{}'",
                    i, channel_layout
                ));
                let channels = stream["channels"].as_u64().unwrap();
                let bitrate = channels * 42;
                next_section = next_section
                    .arg(format!("-b:a:{}", audio_index))
                    .arg(format!("{}K", bitrate));
                audio_index += 1;
            }
        }
        let mut child = next_section
            .arg("-filter_complex")
            .arg(channel_map.join(";"))
            .arg("-c:s")
            .arg(subtitles)
            .arg(format!("{}/audio.mkv", tmp_folder))
            .spawn()
            .unwrap();
        child.wait().await.unwrap();
        permits.add_permits(1);
    })
}

fn vpx_process(
    mut stats_rx: Receiver<FrameStats>,
    scene_buffer: Arc<FrameBuffer>,
    vpx_header: VideoHeader,
    active_encodes_vpx: Arc<Semaphore>,
    vmaf_target: f64,
    cpu_used: u32,
    tmp_folder: String,
) -> JoinHandle<u32> {
    task::spawn(async move {
        let mut scene: u32 = 0;
        let mut file = File::create(format!("{}/{:06}.y4m", tmp_folder, scene))
            .await
            .unwrap();
        let prior_cq_values: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        vpx_header.clone().write(&mut file).await.unwrap();
        while let Ok(stat) = stats_rx.recv().await {
            if stat.is_keyframe {
                file.flush().await.unwrap();
                file.shutdown().await.unwrap();
                drop(file);
                compress_scene(
                    scene,
                    active_encodes_vpx.clone(),
                    prior_cq_values.clone(),
                    vmaf_target,
                    cpu_used,
                    tmp_folder.clone(),
                )
                .await;
                scene += 1;
                file = File::create(format!("{}/{:06}.y4m", tmp_folder, scene))
                    .await
                    .unwrap();
                vpx_header.clone().write(&mut file).await.unwrap();
            }
            let frame = scene_buffer.get_frame(stat.frame_num).await;
            if let Some(frame_data) = frame {
                assert_eq!(stat.frame_num, frame_data.num);
                frame_data.write(&mut file).await.unwrap();
                scene_buffer.pop().await;
            } else {
                break;
            }
        }
        compress_scene(
            scene,
            active_encodes_vpx.clone(),
            prior_cq_values.clone(),
            vmaf_target,
            cpu_used,
            tmp_folder.clone(),
        )
        .await;
        scene
    })
}

async fn compress_scene(
    scene_number: u32,
    encoding_scenes: Arc<Semaphore>,
    prior_cq_values: Arc<Mutex<Vec<u32>>>,
    vmaf_target: f64,
    cpu_used: u32,
    tmp_folder: String,
) -> JoinHandle<()> {
    encoding_scenes.acquire_many(2).await.unwrap().forget();
    tokio::spawn(async move {
        let mut first_pass = first_pass(scene_number, tmp_folder.clone()).await;
        first_pass.wait().await.unwrap();
        let mut initial_min = 20;
        let mut initial_max = 40;
        {
            let guard = prior_cq_values.lock().await;
            if guard.len() >= 10 {
                let sample_point = guard.len() / 10;
                if guard[sample_point] != guard[guard.len() - sample_point - 1] {
                    initial_min = guard[sample_point];
                    initial_max = guard[guard.len() - sample_point - 1];
                }
            }
            drop(guard);
        }
        let cq =
            vmaf_secant_search(10, 60, initial_min, initial_max, vmaf_target, scene_number, tmp_folder.clone()).await;
        encoding_scenes.add_permits(1);
        {
            let mut guard = prior_cq_values.lock().await;
            let insertion_index = guard.binary_search(&cq).unwrap_or_else(|x| x);
            guard.insert(insertion_index, cq);
            drop(guard);
        }
        second_pass(scene_number, cq, cpu_used, tmp_folder.clone())
            .await
            .wait()
            .await
            .unwrap();
        encoding_scenes.add_permits(1);
        cleanup(scene_number, tmp_folder).await;
    })
}

async fn first_pass(scene_number: u32, tmp_folder: String) -> Child {
    let scene_str = format!("{}/{:06}", tmp_folder, scene_number);
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

async fn second_pass(scene_number: u32, cq: u32, cpu_used: u32, tmp_folder: String) -> Child {
    let scene_str = format!("{}/{:06}", tmp_folder, scene_number);
    Command::new("vpxenc")
        .arg(format!("--cq-level={}", cq))
        .arg(format!("--cpu-used={}", cpu_used))
        .arg(format!("--fpf={}.log", scene_str))
        .arg("--quiet")
        .arg("--passes=2")
        .arg("--pass=2")
        .arg("--profile=2")
        .arg("--good")
        .arg("--lag-in-frames=25")
        .arg("--kf-max-dist=250")
        .arg("--auto-alt-ref=1")
        .arg("--arnr-strength=1")
        .arg("--arnr-maxframes=7")
        .arg("--enable-tpl=1")
        .arg("--threads=1")
        .arg("-b")
        .arg("10")
        .arg("--end-usage=q")
        .arg("--ivf")
        .arg("-o")
        .arg(format!("{}.ivf", scene_str))
        .arg(format!("{}.y4m", scene_str))
        .spawn()
        .unwrap()
}

async fn cleanup(scene_number: u32, tmp_folder: String) {
    let scene_str = format!("{}/{:06}", tmp_folder, scene_number);
    let remove_video = remove_file(format!("{}.y4m", scene_str));
    let remove_scene = remove_file(format!("{}.log", scene_str));
    let (video, scene) = join!(remove_video, remove_scene);
    video.unwrap();
    scene.unwrap();
}

async fn vmaf_second_pass(scene_number: u32, cq: u32, cpu_used: u32, tmp_folder: String) -> f64 {
    let scene_str = format!("{}/{:06}", tmp_folder, scene_number);
    let threads = if cpu_used > 3 { 1 } else { 2 };
    let mut vpx = Command::new("vpxenc")
        .arg(format!("--cq-level={}", cq))
        .arg(format!("--cpu-used={}", cpu_used))
        .arg(format!("--fpf={}.log", scene_str))
        .arg(format!("--threads={}", threads))
        .arg("--quiet")
        .arg("--passes=2")
        .arg("--pass=2")
        .arg("--profile=2")
        .arg("-b")
        .arg("10")
        .arg("--end-usage=q")
        .arg("--ivf")
        .arg("--row-mt=1")
        .arg("--tile-columns=1")
        .arg("-o")
        .arg("-")
        .arg(format!("{}.y4m", scene_str))
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    let vpx_stdin: Stdio = vpx
        .stdout
        .take()
        .unwrap()
        .try_into()
        .expect("failed to convert to Stdio");

    let ffmpeg = Command::new("ffmpeg")
        .stdin(vpx_stdin)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .arg("-y")
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
    tmp_folder: String,
) -> u32 {
    let mut x1 = initial_guess_min;
    let mut x2 = initial_guess_max;
    let f = tmp_folder.clone();
    let f2 = tmp_folder.clone();
    let first_fx1 = task::spawn(async move { vmaf_second_pass(scene_number, x1, 6, f.clone()).await });
    let first_fx2 = task::spawn(async move { vmaf_second_pass(scene_number, x2, 6, f2.clone()).await });
    let (fx1_result, fx2_result) = join!(first_fx1, first_fx2);
    let fx1_target = fx1_result.unwrap() - target;
    let mut fx1 = fx1_target;
    let mut fx2 = fx2_result.unwrap() - target;
    // If vmaf for the second value is greater then the target, then we want it pinned so future guess aren't lower than this first high guess.
    // For example, if the guess is 40 with vmaf of 99 and a target of 95, a guess of 60 might return 80, which would make the next guess less than 40 (since it falls off naturally as a result of secant searching)
    // This swap keeps the 40 for the next pass which will make the next guess > 40.
    // This only applies to the first pass as subsequent guesses should always narrow into the right result.
    if fx2.abs() < fx1.abs() {
        fx1 = fx2;
        x1 = x2;
        fx2 = fx1_target;
        x2 = initial_guess_min;
    }
    let mut iterations = 0;
    while fx1.abs() > 0.005 && iterations < 10 {
        let mut next = (x1 as f64 - (fx1 * ((x1 as f64 - x2 as f64) / (fx1 - fx2)))).floor() as u32;
        println!(
            "{}({}): {}:{} {}:{} -> {}",
            scene_number,
            iterations,
            x1,
            fx1 + target,
            x2,
            fx2 + target,
            next
        );
        x2 = x1;
        fx2 = fx1;
        if next < min {
            if x1 == min {
                // We already tried min.  No reason to try again.
                break;
            } else {
                next = min
            }
        } else if next > max {
            if x1 == max {
                // We already tried max, don't try it again.
                break;
            } else {
                next = max
            }
        } else if next == x1 {
            // We are so close that the next guess ends up being the current guess, just jump out
            break;
        }
        x1 = next;
        fx1 = vmaf_second_pass(scene_number, x1, 3, tmp_folder.clone()).await - target;
        iterations += 1;
    }
    println!("{}: {}:{}", scene_number, x1, fx1 + target);
    if fx1 > 0.0 {
        x1
    } else {
        (x1 - 1).max(min)
    }
}

fn stats_processor(
    video_header: VideoHeader,
    delayed_aom: Arc<Semaphore>,
    stats_tx: Sender<FrameStats>,
    tmp_folder: String,
) -> JoinHandle<()> {
    task::spawn(async move {
        delayed_aom.acquire_many(96).await.unwrap().forget();
        let mut frame_stats = VecDeque::new();
        let mut keyframe =
            BufReader::with_capacity(1024, File::open(format!("{}/keyframe.log", tmp_folder).as_str()).await.unwrap());
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
            if since_last_keyframe >= 1000
                || current.test_candidate_kf(&last, &frame_stats, since_last_keyframe, num_mbs)
            {
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
    })
}

fn aom_firstpass_for_scene_detection(
    video_header: VideoHeader,
    analyzed_aom_frames: Arc<Semaphore>,
    writing_buf: Arc<FrameBuffer>,
    tmp_folder: String,
) -> JoinHandle<()> {
    let mut aom = start_aom_scene_detection(tmp_folder.clone());
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

fn start_aom_scene_detection(tmp_folder: String) -> Child {
    Command::new("nice")
        .arg("-20")
        .arg("aomenc")
        .arg("--passes=2")
        .arg("--pass=1")
        .arg("--bit-depth=10")
        .arg(format!("--fpf={}/keyframe.log", tmp_folder))
        .arg("--end-usage=q")
        .arg("--threads=4")
        .arg("-o")
        .arg("/dev/null")
        .arg("-")
        .stdin(Stdio::piped())
        .spawn()
        .unwrap()
}

fn start_vspipe(input: &str, vpy: &str, tmp_folder: String) -> Child {
    Command::new("nice")
        .arg("-20")
        .arg("vspipe")
        .arg("-c")
        .arg("y4m")
        .arg("-t")
        .arg(format!("{}/timecodes.txt", tmp_folder))
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
                .help("Input file")
                .required(true)
                .multiple_values(true)
                .takes_value(true),
        )
        .arg(
            Arg::new("vpy")
                .short('v')
                .long("vpy")
                .help("vapoursynth file")
                .required(true)
                .multiple_values(false)
                .takes_value(true),
        )
        .arg(
            Arg::new("encoders")
                .short('e')
                .long("encoders")
                .help("Number of encoders")
                .default_value("12")
                .multiple_values(false)
                .takes_value(true),
        )
        .arg(
            Arg::new("vmaf_target")
                .short('t')
                .long("vmaf_target")
                .default_value("95")
                .multiple_values(false)
                .takes_value(true),
        )
        .arg(
            Arg::new("cpu_used")
                .short('c')
                .long("cpu_used")
                .default_value("0")
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
    (mb_rows * mb_cols) as u32
}

fn align_power_of_two(value: i32, n: i32) -> i32 {
    let x = value + ((1 << n) - 1);
    let y = ((1 << n) - 1).not();
    x.bitand(y)
}

#[derive(Clone)]
struct FrameStats {
    frame_num: u64,
    is_keyframe: bool,
}

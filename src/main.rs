mod aom_firstpass;
mod av1_encoder;
mod encoder;
mod frame;
mod frame_buffer;
mod shutdown;
mod video_header;
mod vp9_encoder;
mod x265_encoder;

use crate::aom_firstpass::aom::AomFirstpass;
use crate::frame::Frame;
use crate::frame::Status::Processing;
use crate::frame_buffer::FrameBuffer;
use crate::shutdown::TrackedChild;
use crate::video_header::VideoHeader;
use clap::{value_parser, Arg, ArgMatches, Command as ClapCommand};
use regex::{Captures, Regex};
use serde_json::Value;

use crate::av1_encoder::Av1Encoder;
use crate::encoder::{Encoder, EncoderOptions};
use crate::vp9_encoder::Vp9Encoder;
use crate::x265_encoder::X265Encoder;
use glob::glob;
use std::collections::VecDeque;
use std::convert::TryInto;
use std::io::SeekFrom;
use std::ops::{BitAnd, Not};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use tokio::fs::File;
use tokio::fs::{create_dir, remove_dir_all, remove_file};
use tokio::io::{AsyncSeekExt, AsyncWriteExt, BufReader, ErrorKind};
use tokio::join;
use tokio::process::Command;
use tokio::sync::{mpsc, Mutex, Semaphore};
use tokio::task;
use tokio::task::JoinHandle;

use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[tokio::main]
async fn main() {
    shutdown::install();
    let options = extract_options();

    let targets: Vec<PathBuf> = options
        .get_many::<String>("input")
        .expect("At least one input is required")
        .flat_map(move |f| glob(f.as_str()).unwrap())
        .map(move |f| f.unwrap())
        .collect();

    println!("Encoding {} files", targets.len());

    let encoders = *options.get_one::<usize>("encoders").unwrap();
    let cpu_used = *options.get_one::<u32>("cpu_used").unwrap();
    let vmaf_cpu_used = *options.get_one::<u32>("vmaf_cpu_used").unwrap();
    let vpy: String = options.get_one::<String>("vpy").unwrap().clone();
    let vmaf_target: f64 = *options.get_one::<f64>("vmaf_target").unwrap() / 100.0;
    let encoder_str: String = options.get_one::<String>("codec").unwrap().clone();
    let encoder: Arc<dyn Encoder + Send + Sync> = match encoder_str.as_str() {
        "vpx" => Arc::new(Vp9Encoder {}),
        "av1" => Arc::new(Av1Encoder {}),
        "x265" => Arc::new(X265Encoder {}),
        _ => panic!("Shouldn't have gotten here"),
    };
    let active_encodes = Arc::new(Semaphore::new(encoders));

    let mut tasks = vec![];
    let can_do_next = Arc::new(Semaphore::new(0));
    for entry in targets {
        let len = tasks.len();

        let vpy = vpy.clone();
        let active_encodes = active_encodes.clone();
        let entry = entry.clone();
        let cdn = can_do_next.clone();
        let e = encoder.clone();

        tasks.push(tokio::spawn(async move {
            compress_file(
                cpu_used,
                vmaf_cpu_used,
                vpy,
                vmaf_target,
                active_encodes,
                entry,
                cdn,
                len,
                e,
            )
            .await
        }));
        can_do_next.acquire().await.unwrap().forget()
    }

    for task in tasks {
        match task.await {
            Ok(()) => {}
            Err(_) if shutdown::is_shutting_down() => {
                std::future::pending::<()>().await;
            }
            Err(error) => std::panic::resume_unwind(error.into_panic()),
        }
    }
    if shutdown::is_shutting_down() {
        std::future::pending::<()>().await;
    }
}

async fn compress_file(
    cpu_used: u32,
    vmaf_cpu_used: u32,
    vpy: String,
    vmaf_target: f64,
    active_encoders: Arc<Semaphore>,
    input_path: PathBuf,
    can_do_next: Arc<Semaphore>,
    processed_file: usize,
    encoder: Arc<dyn Encoder + Send + Sync>,
) {
    let i: String = input_path.to_str().unwrap().to_string();
    println!("Encoding {}", i);
    let tmp_folder = format!("/tmp/{}", processed_file);
    create_dir(&tmp_folder).await.unwrap();
    if !shutdown::track_temp_dir(PathBuf::from(&tmp_folder)) {
        let _ = remove_dir_all(&tmp_folder).await;
        return;
    }

    let audio_processing = encode_audio(i.clone(), active_encoders.clone(), tmp_folder.clone());

    let mut vspipe = start_vspipe(i.clone().as_str(), vpy.as_str(), tmp_folder.clone());
    let vspipe_output = vspipe.stdout.take().unwrap();
    let mut vs_pipe_reader = BufReader::with_capacity(1024, vspipe_output);

    let analyzed_aom_frames = Arc::new(Semaphore::new(0));
    let header = VideoHeader::read(&mut vs_pipe_reader).await.unwrap();

    let buffer = Arc::new(FrameBuffer::new(129, header.clone()));

    let delayed_aom = analyzed_aom_frames.clone();
    let aom_finished = Arc::new(AtomicBool::new(false));
    let (stats_tx, stats_rx) = mpsc::channel(129);

    let frame_stats_processor = stats_processor(
        header.clone(),
        delayed_aom,
        aom_finished.clone(),
        stats_tx,
        tmp_folder.clone(),
    );

    let container_format = encoder.container_format();
    let processing = process(
        stats_rx,
        buffer.clone(),
        header.clone(),
        active_encoders.clone(),
        vmaf_target,
        cpu_used,
        vmaf_cpu_used,
        tmp_folder.clone(),
        encoder,
    );

    let aom_first_pass_scene = aom_firstpass_for_scene_detection(
        header,
        analyzed_aom_frames,
        aom_finished,
        buffer.clone(),
        tmp_folder.clone(),
    );

    let mut status = buffer.read_in_frame(&mut vs_pipe_reader).await.unwrap();
    while status == Processing {
        status = buffer.read_in_frame(&mut vs_pipe_reader).await.unwrap();
    }
    drop(vs_pipe_reader);
    vspipe.wait().await.unwrap();
    can_do_next.add_permits(1);
    let scenes = processing.await.unwrap();
    aom_first_pass_scene.await.unwrap();
    frame_stats_processor.await.unwrap();
    audio_processing.await.unwrap();

    concat(input_path, tmp_folder.clone(), scenes, container_format).await;
    println!("Cleaning up temp folder");
    remove_dir_all(&tmp_folder).await.unwrap();
    shutdown::untrack_temp_dir(Path::new(&tmp_folder));
}

fn mkvmerge_options(
    output_name: &str,
    tmp_folder: &str,
    scenes: u32,
    container_format: &str,
    has_timecodes: bool,
) -> Vec<String> {
    let mut options: Vec<String> = Vec::new();
    options.push("-o".to_string());
    options.push(output_name.to_string());

    options.push(format!("{}/audio.mkv", tmp_folder));

    if scenes > 0 {
        let append_to: Vec<String> = (1..=scenes)
            .map(|scene| format!("{}:0:{}:0", scene + 1, scene))
            .collect();
        options.push("--append-to".to_string());
        options.push(append_to.join(","));
    }

    if has_timecodes {
        options.push("--timestamps".to_string());
        options.push(format!("0:{}/timecodes.txt", tmp_folder));
    }

    for scene in 0..=scenes {
        let video_file = format!("{}/{:06}.{}", tmp_folder, scene, container_format);
        if scene == 0 {
            options.push(video_file);
        } else {
            options.push(format!("+{}", video_file));
        }
    }

    options
}

async fn concat(input_path: PathBuf, tmp_folder: String, scenes: u32, container_format: &str) {
    let output_name = String::from(
        input_path
            .with_extension("new.mkv")
            .file_name()
            .unwrap()
            .to_str()
            .unwrap(),
    );

    let has_timecodes = Path::new(format!("{}/timecodes.txt", tmp_folder).as_str()).exists();
    let options = mkvmerge_options(
        &output_name,
        &tmp_folder,
        scenes,
        container_format,
        has_timecodes,
    );

    serde_json::to_writer(
        &std::fs::File::create(format!("{}/options.json", tmp_folder).as_str())
            .expect("could not create options file"),
        &options,
    )
    .expect("Failed to serialize options file");

    println!("Writing {}", output_name);
    let mut mkvmerge = Command::new("mkvmerge");
    mkvmerge.arg(format!("@{}/options.json", tmp_folder).as_str());
    let status = TrackedChild::spawn(mkvmerge)
        .expect("failed to start mkvmerge")
        .wait()
        .await
        .expect("failed to wait for mkvmerge");

    match status.code() {
        Some(0) => {}
        Some(1) => eprintln!("mkvmerge completed with warnings while writing {output_name}"),
        _ => {
            let _ = std::fs::remove_file(&output_name);
            panic!("mkvmerge failed with {status} while writing {output_name}");
        }
    }
}

fn encode_audio(i: String, permits: Arc<Semaphore>, tmp_folder: String) -> JoinHandle<()> {
    task::spawn(async move {
        permits.acquire().await.unwrap().forget();
        let mut ffprobe = Command::new("ffprobe");
        ffprobe
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .arg("-hide_banner")
            .arg("-print_format")
            .arg("json")
            .arg("-show_streams")
            .arg(&i);
        let probe_results = TrackedChild::spawn(ffprobe)
            .unwrap()
            .wait_with_output()
            .await
            .unwrap();

        let probe_result: Value = serde_json::from_slice(&probe_results.stdout).unwrap();
        let streams = probe_result["streams"].as_array().unwrap();

        let audio_encode = audio_encode_command(&i, streams, &tmp_folder);
        let mut child = TrackedChild::spawn(audio_encode).unwrap();
        child.wait().await.unwrap();
        permits.add_permits(1);
    })
}

fn audio_encode_command(input: &str, streams: &[Value], tmp_folder: &str) -> Command {
    let mut audio_encode = Command::new("ffmpeg");
    let subtitles = if input.ends_with("mp4") {
        "srt"
    } else {
        "copy"
    };
    audio_encode
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::null())
        .arg("-nostdin")
        .arg("-y")
        .arg("-i")
        .arg(input)
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
            audio_encode
                .arg(format!("-b:a:{}", audio_index))
                .arg(format!("{}K", bitrate));
            audio_index += 1;
        }
    }
    audio_encode
        .arg("-filter_complex")
        .arg(channel_map.join(";"))
        .arg("-c:s")
        .arg(subtitles)
        .arg(format!("{}/audio.mkv", tmp_folder));
    audio_encode
}

fn process(
    mut stats_rx: mpsc::Receiver<FrameStats>,
    scene_buffer: Arc<FrameBuffer>,
    header: VideoHeader,
    active_encodes_vpx: Arc<Semaphore>,
    vmaf_target: f64,
    cpu_used: u32,
    vmaf_cpu_used: u32,
    tmp_folder: String,
    encoder: Arc<dyn Encoder + Send + Sync>,
) -> JoinHandle<u32> {
    task::spawn(async move {
        let header = Arc::new(header);
        let mut scene: u32 = 0;
        let mut file = create_scene_file(&tmp_folder, scene, header.clone()).await;
        let prior_cq_values: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
        let mut inflight_scenes = vec![];
        while let Some(stat) = stats_rx.recv().await {
            if stat.is_keyframe {
                let next_file = create_scene_file(&tmp_folder, scene + 1, header.clone()).await;
                inflight_scenes.push(
                    compress_scene(
                        scene,
                        std::mem::replace(&mut file, next_file).close(),
                        active_encodes_vpx.clone(),
                        prior_cq_values.clone(),
                        vmaf_target,
                        cpu_used,
                        vmaf_cpu_used,
                        tmp_folder.clone(),
                        encoder.clone(),
                    )
                    .await,
                );
                scene += 1;
            }
            let frame = scene_buffer.get_frame(stat.frame_num).await;
            if let Some(frame_data) = frame {
                assert_eq!(stat.frame_num, frame_data.num);
                file = write_scene_frame(file, frame_data).await;
                scene_buffer.pop().await;
            } else {
                break;
            }
        }
        println!("Compressing final scene");
        inflight_scenes.push(
            compress_scene(
                scene,
                file.close(),
                active_encodes_vpx.clone(),
                prior_cq_values.clone(),
                vmaf_target,
                cpu_used,
                vmaf_cpu_used,
                tmp_folder.clone(),
                encoder,
            )
            .await,
        );
        for scene in inflight_scenes {
            scene.await.unwrap();
        }
        scene
    })
}

struct SceneWriter {
    child: TrackedChild,
    input: tokio::process::ChildStdin,
}

impl SceneWriter {
    fn close(self) -> SceneFile {
        drop(self.input);
        SceneFile(self.child)
    }
}

struct SceneFile(TrackedChild);

impl SceneFile {
    async fn wait(mut self) {
        let status = self.0.wait().await.unwrap();
        assert!(status.success(), "ffmpeg failed while writing a scene");
    }
}

const FFV1_DECODE_THREADS: &str = "4";

fn ffv1_scene_command(path: &str) -> Command {
    let mut command = Command::new("ffmpeg");
    command
        .arg("-nostdin")
        .arg("-y")
        .arg("-loglevel")
        .arg("error")
        .arg("-f")
        .arg("yuv4mpegpipe")
        .arg("-i")
        .arg("pipe:0")
        .arg("-c:v")
        .arg("ffv1")
        .arg("-f")
        .arg("matroska")
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null());
    command
}

fn ffv1_decoder_command(path: &str) -> Command {
    let mut command = Command::new("ffmpeg");
    command
        .arg("-nostdin")
        .arg("-loglevel")
        .arg("error")
        .arg("-threads")
        .arg(FFV1_DECODE_THREADS)
        .arg("-i")
        .arg(path)
        // FFmpeg's y4m muxer rejects 10-bit pixel formats without this.
        .arg("-strict")
        .arg("-1")
        .arg("-f")
        .arg("yuv4mpegpipe")
        .arg("pipe:1")
        .stdout(Stdio::piped());
    command
}

fn start_ffv1_decoder(path: &str) -> TrackedChild {
    TrackedChild::spawn(ffv1_decoder_command(path)).expect("failed to start ffmpeg decoder")
}

async fn create_scene_file(tmp_folder: &str, scene: u32, header: Arc<VideoHeader>) -> SceneWriter {
    let path = format!("{}/{:06}.mkv", tmp_folder, scene);
    let mut child = TrackedChild::spawn(ffv1_scene_command(&path)).unwrap();
    let mut input = child.stdin.take().unwrap();
    input.write_all(&header.as_bytes).await.unwrap();
    SceneWriter { child, input }
}

async fn write_scene_frame(mut writer: SceneWriter, frame: Arc<Frame>) -> SceneWriter {
    writer.input.write_all(b"FRAME\x0A").await.unwrap();
    writer.input.write_all(frame.data()).await.unwrap();
    writer
}

struct EncodingProcess {
    encoder: TrackedChild,
    decoder: TrackedChild,
}

impl EncodingProcess {
    async fn wait(&mut self) {
        let encoder = self.encoder.wait().await.unwrap();
        let decoder = self.decoder.wait().await.unwrap();
        assert!(encoder.success(), "encoder failed");
        assert!(decoder.success(), "ffmpeg decoder failed");
    }
}

async fn compress_scene(
    scene_number: u32,
    scene_file: SceneFile,
    encoding_scenes: Arc<Semaphore>,
    prior_cq_values: Arc<Mutex<Vec<u32>>>,
    vmaf_target: f64,
    cpu_used: u32,
    vmaf_cpu_used: u32,
    tmp_folder: String,
    encoder: Arc<dyn Encoder + Send + Sync>,
) -> JoinHandle<()> {
    encoding_scenes.acquire_many(2).await.unwrap().forget();
    tokio::spawn(async move {
        scene_file.wait().await;
        if let Some(mut first_pass) =
            first_pass(scene_number, tmp_folder.clone(), encoder.clone()).await
        {
            first_pass.wait().await;
        }
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
        let cq = vmaf_secant_search(
            10,
            encoder.max_cq(),
            initial_min,
            initial_max,
            vmaf_cpu_used,
            vmaf_target,
            scene_number,
            tmp_folder.clone(),
            encoder.clone(),
        )
        .await;
        encoding_scenes.add_permits(1);
        {
            let mut guard = prior_cq_values.lock().await;
            let insertion_index = guard.binary_search(&cq).unwrap_or_else(|x| x);
            guard.insert(insertion_index, cq);
            drop(guard);
        }
        let mut second = second_pass(
            scene_number,
            cq,
            cpu_used,
            tmp_folder.clone(),
            encoder.clone(),
        )
        .await;
        second.wait().await;
        drop(second);
        encoding_scenes.add_permits(1);
        cleanup(scene_number, tmp_folder).await;
    })
}

async fn first_pass(
    scene_number: u32,
    tmp_folder: String,
    encoder: Arc<dyn Encoder + Send + Sync>,
) -> Option<EncodingProcess> {
    let scene_str = format!("{}/{:06}", tmp_folder, scene_number);
    let scene_path = format!("{}.mkv", scene_str);
    let mut command = encoder.first_pass(EncoderOptions {
        log_file: format!("{}.log", scene_str).as_str(),
        input: "-",
        output: "/dev/null",
        ..Default::default()
    })?;
    let mut decoder = start_ffv1_decoder(&scene_path);
    let decoder_stdout: Stdio = decoder.stdout.take().unwrap().try_into().unwrap();
    command.stdin(decoder_stdout);
    Some(EncodingProcess {
        encoder: TrackedChild::spawn(command).unwrap(),
        decoder,
    })
}

async fn second_pass(
    scene_number: u32,
    cq: u32,
    cpu_used: u32,
    tmp_folder: String,
    encoder: Arc<dyn Encoder + Send + Sync>,
) -> EncodingProcess {
    let scene_str = format!("{}/{:06}", tmp_folder, scene_number);
    let scene_path = format!("{}.mkv", scene_str);
    let mut command = encoder.second_pass(EncoderOptions {
        cq,
        cpu_used,
        log_file: format!("{}.log", scene_str).as_str(),
        input: "-",
        output: format!("{}.{}", scene_str, encoder.container_format()).as_str(),
        ..Default::default()
    });
    let mut decoder = start_ffv1_decoder(&scene_path);
    let decoder_stdout: Stdio = decoder.stdout.take().unwrap().try_into().unwrap();
    command.stdin(decoder_stdout);
    EncodingProcess {
        encoder: TrackedChild::spawn(command).unwrap(),
        decoder,
    }
}

async fn cleanup(scene_number: u32, tmp_folder: String) {
    let scene_str = format!("{}/{:06}", tmp_folder, scene_number);
    let remove_video = remove_file(format!("{}.mkv", scene_str));
    let remove_scene = remove_file(format!("{}.log", scene_str));
    let (video, scene) = join!(remove_video, remove_scene);
    video.unwrap();
    if let Err(e) = scene {
        if e.kind() != ErrorKind::NotFound {
            panic!("Failed to remove {}.log: {}", scene_str, e);
        }
    }
}

async fn vmaf_second_pass(
    scene_number: u32,
    cq: u32,
    cpu_used: u32,
    threads: u32,
    tmp_folder: String,
    encoder: Arc<dyn Encoder + Send + Sync>,
) -> f64 {
    let scene_str = format!("{}/{:06}", tmp_folder, scene_number);
    let scene_path = format!("{}.mkv", scene_str);
    let mut command = encoder.second_pass(EncoderOptions {
        cq,
        cpu_used,
        threads,
        log_file: format!("{}.log", scene_str).as_str(),
        output: "-",
        input: "-",
        ..Default::default()
    });
    let mut decoder = start_ffv1_decoder(&scene_path);
    let decoder_stdout: Stdio = decoder.stdout.take().unwrap().try_into().unwrap();
    command.stdin(decoder_stdout);
    command.stdout(Stdio::piped());
    let mut encode = TrackedChild::spawn(command).unwrap();

    let encode_stdin: Stdio = encode
        .stdout
        .take()
        .unwrap()
        .try_into()
        .expect("failed to convert to Stdio");

    let mut ffmpeg_command = Command::new("ffmpeg");
    ffmpeg_command
        .stdin(encode_stdin)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .arg("-y")
        .arg("-f")
        .arg(encoder.container_format())
        .arg("-i")
        .arg("pipe:0")
        .arg("-threads")
        .arg(FFV1_DECODE_THREADS)
        .arg("-i")
        .arg(&scene_path)
        .arg("-threads")
        .arg("1")
        .arg("-lavfi")
        .arg("libvmaf=model='path=/usr/local/share/model/vmaf_v0.6.1.json'")
        .arg("-f")
        .arg("null")
        .arg("-");
    let ffmpeg = TrackedChild::spawn(ffmpeg_command).unwrap();

    let (encode_status, decoder_status, ffmpeg_output) =
        join!(encode.wait(), decoder.wait(), ffmpeg.wait_with_output());
    assert!(encode_status.unwrap().success(), "encoder failed");
    assert!(decoder_status.unwrap().success(), "ffmpeg decoder failed");

    static VMAF_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"VMAF score:\s+([\d|.]+)").unwrap());
    let results = String::from_utf8(ffmpeg_output.unwrap().stderr).unwrap();
    let captures: Captures = VMAF_RE
        .captures(results.as_str())
        .unwrap_or_else(|| panic!("Failed to decode: {}", results));
    let capture: &str = &captures[1];

    capture.parse().map(|n: f64| n / 100.0).unwrap()
}

async fn vmaf_secant_search(
    min: u32,
    max: u32,
    initial_guess_min: u32,
    initial_guess_max: u32,
    vmaf_cpu_used: u32,
    target: f64,
    scene_number: u32,
    tmp_folder: String,
    encoder: Arc<dyn Encoder + Send + Sync>,
) -> u32 {
    let mut x1 = initial_guess_min;
    let mut x2 = initial_guess_max;
    let f = tmp_folder.clone();
    let f2 = tmp_folder.clone();
    let e1 = encoder.clone();
    let e2 = encoder.clone();
    let first_fx1 =
        task::spawn(async move { vmaf_second_pass(scene_number, x1, 6, 1, f.clone(), e1).await });
    let first_fx2 =
        task::spawn(async move { vmaf_second_pass(scene_number, x2, 6, 1, f2.clone(), e2).await });
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
            "{}({}): {}:{} {}:{} → {}",
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
        if x1 == max && fx1 > 0.0 {
            break;
        }
        x1 = next;
        fx1 = vmaf_second_pass(
            scene_number,
            x1,
            vmaf_cpu_used,
            2,
            tmp_folder.clone(),
            encoder.clone(),
        )
        .await
            - target;
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
    aom_finished: Arc<AtomicBool>,
    stats_tx: mpsc::Sender<FrameStats>,
    tmp_folder: String,
) -> JoinHandle<()> {
    task::spawn(async move {
        delayed_aom.acquire_many(112).await.unwrap().forget();
        let mut frame_stats = VecDeque::new();
        let mut keyframe_file = File::open(format!("{}/keyframe.log", tmp_folder).as_str())
            .await
            .unwrap();
        let format = AomFirstpass::detect_format(&mut keyframe_file, &aom_finished)
            .await
            .expect("Unsupported AOM first pass stats layout");
        keyframe_file.seek(SeekFrom::Start(0)).await.unwrap();
        let mut keyframe = BufReader::with_capacity(1024, keyframe_file);
        let mut current = AomFirstpass::read_aom_firstpass(&mut keyframe, format, &aom_finished)
            .await
            .unwrap();
        let mut last = current;
        let mut reached_end = false;
        // Fill up the frame buffer
        while frame_stats.len() < 32 {
            match AomFirstpass::read_aom_firstpass(&mut keyframe, format, &aom_finished).await {
                Ok(stat) => frame_stats.push_back(stat),
                Err(e) if e.kind() == ErrorKind::UnexpectedEof => {
                    // aomenc ends the log with a whole-clip summary record.
                    frame_stats.pop_back();
                    reached_end = true;
                    break;
                }
                Err(e) => panic!("Exploded unexpectedly! {}", e),
            }
        }
        let mut since_last_keyframe = 1;
        let num_mbs = mbs(video_header.width, video_header.height);
        loop {
            delayed_aom.acquire().await.unwrap().forget();
            let scene_cut = !reached_end
                && !frame_stats.is_empty()
                && current.test_candidate_kf(
                    &last,
                    &frame_stats,
                    since_last_keyframe,
                    num_mbs,
                    format,
                )
                && (!format.has_lt_coded_error() || !current.should_test_next_gop(&frame_stats));
            if since_last_keyframe >= 1000 || scene_cut {
                let _ = stats_tx
                    .send(FrameStats {
                        frame_num: current.frame as u64,
                        is_keyframe: true,
                    })
                    .await;
                since_last_keyframe = 0;
            } else {
                let _ = stats_tx
                    .send(FrameStats {
                        frame_num: current.frame as u64,
                        is_keyframe: false,
                    })
                    .await;
            }
            since_last_keyframe += 1;
            last = current;
            let Some(next) = frame_stats.pop_front() else {
                break;
            };
            current = next;
            if reached_end {
                continue;
            }
            match AomFirstpass::read_aom_firstpass(&mut keyframe, format, &aom_finished).await {
                Ok(stat) => frame_stats.push_back(stat),
                Err(e) => {
                    if e.kind() == ErrorKind::UnexpectedEof {
                        // aomenc ends the log with a whole-clip summary record.
                        frame_stats.pop_back();
                        reached_end = true;
                    } else {
                        panic!("Exploded unexpectedly! {}", e)
                    }
                }
            }
        }
        drop(stats_tx);
    })
}

fn aom_firstpass_for_scene_detection(
    video_header: VideoHeader,
    analyzed_aom_frames: Arc<Semaphore>,
    aom_finished: Arc<AtomicBool>,
    writing_buf: Arc<FrameBuffer>,
    tmp_folder: String,
) -> JoinHandle<()> {
    let mut aom = start_aom_scene_detection(tmp_folder.clone());
    let mut aom_input = aom.stdin.take().unwrap();
    task::spawn(async move {
        video_header.write(&mut aom_input).await.unwrap();
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
                aom_finished.store(true, Ordering::Release);
                // Allows for the stats to process to the end
                analyzed_aom_frames.add_permits(99999);
                break;
            }
        }
    })
}

fn start_aom_scene_detection(tmp_folder: String) -> TrackedChild {
    let mut command = Command::new("nice");
    command
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
        .stdin(Stdio::piped());
    TrackedChild::spawn(command).unwrap()
}

fn start_vspipe(input: &str, vpy: &str, tmp_folder: String) -> TrackedChild {
    let mut command = Command::new("nice");
    command
        .arg("-20")
        .arg("vspipe")
        .arg("-c")
        .arg("y4m")
        .arg("-t")
        .arg(format!("{}/timecodes.txt", tmp_folder))
        .arg("--arg")
        .arg(format!("file={}", input))
        .arg("--arg")
        .arg(format!("tmp_folder={}", tmp_folder))
        .arg(vpy)
        .arg("-")
        .stdout(Stdio::piped());
    TrackedChild::spawn(command).unwrap()
}

fn cli() -> ClapCommand {
    ClapCommand::new("sav1n")
        .version("0.0.1")
        .author("Thomas May")
        .arg(
            Arg::new("input")
                .short('i')
                .long("input")
                .help("Input file")
                .required(true)
                .num_args(1..),
        )
        .arg(
            Arg::new("vpy")
                .short('v')
                .long("vpy")
                .help("vapoursynth file")
                .required(true),
        )
        .arg(
            Arg::new("encoders")
                .short('e')
                .long("encoders")
                .help("Number of encoders")
                .default_value("12")
                .value_parser(value_parser!(usize)),
        )
        .arg(
            Arg::new("vmaf_target")
                .short('t')
                .long("vmaf_target")
                .default_value("95")
                .value_parser(value_parser!(f64)),
        )
        .arg(
            Arg::new("cpu_used")
                .short('c')
                .long("cpu_used")
                .default_value("0")
                .value_parser(value_parser!(u32)),
        )
        .arg(
            Arg::new("vmaf_cpu_used")
                .long("vmaf_cpu_used")
                .default_value("3")
                .value_parser(value_parser!(u32)),
        )
        .arg(
            Arg::new("codec")
                .short('o')
                .long("codec")
                .default_value("vpx")
                .value_parser(["vpx", "av1", "x265"]),
        )
}

fn extract_options() -> ArgMatches {
    cli().get_matches()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_definition_is_valid() {
        cli().debug_assert();
    }

    #[test]
    fn parses_required_and_default_options() {
        let matches = cli()
            .try_get_matches_from(["sav1n", "-i", "one.mkv", "two.mkv", "-v", "script.vpy"])
            .unwrap();

        let inputs: Vec<&str> = matches
            .get_many::<String>("input")
            .unwrap()
            .map(String::as_str)
            .collect();
        assert_eq!(inputs, ["one.mkv", "two.mkv"]);
        assert_eq!(matches.get_one::<String>("vpy").unwrap(), "script.vpy");
        assert_eq!(*matches.get_one::<usize>("encoders").unwrap(), 12);
        assert_eq!(*matches.get_one::<f64>("vmaf_target").unwrap(), 95.0);
        assert_eq!(*matches.get_one::<u32>("cpu_used").unwrap(), 0);
        assert_eq!(*matches.get_one::<u32>("vmaf_cpu_used").unwrap(), 3);
        assert_eq!(matches.get_one::<String>("codec").unwrap(), "vpx");
    }

    #[test]
    fn parses_typed_options() {
        let matches = cli()
            .try_get_matches_from([
                "sav1n",
                "-i",
                "input.mkv",
                "-v",
                "script.vpy",
                "-e",
                "4",
                "-t",
                "92",
                "-c",
                "2",
                "--vmaf_cpu_used",
                "5",
                "-o",
                "x265",
            ])
            .unwrap();

        assert_eq!(*matches.get_one::<usize>("encoders").unwrap(), 4);
        assert_eq!(*matches.get_one::<f64>("vmaf_target").unwrap(), 92.0);
        assert_eq!(*matches.get_one::<u32>("cpu_used").unwrap(), 2);
        assert_eq!(*matches.get_one::<u32>("vmaf_cpu_used").unwrap(), 5);
        assert_eq!(matches.get_one::<String>("codec").unwrap(), "x265");
    }

    #[test]
    fn rejects_unknown_codec() {
        let result = cli().try_get_matches_from([
            "sav1n",
            "-i",
            "input.mkv",
            "-v",
            "script.vpy",
            "-o",
            "h264",
        ]);

        assert!(result.is_err());
    }

    #[test]
    fn audio_encode_does_not_read_from_stdin() {
        let streams = [
            serde_json::json!({"codec_type": "video"}),
            serde_json::json!({"codec_type": "audio", "channel_layout": "5.1(side)", "channels": 6}),
        ];

        let command = audio_encode_command("video.mkv", &streams, "/tmp/0");
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect();

        assert_eq!(
            args,
            [
                "-nostdin",
                "-y",
                "-i",
                "video.mkv",
                "-map",
                "0",
                "-vn",
                "-c:a",
                "libopus",
                "-vbr",
                "on",
                "-b:a:0",
                "252K",
                "-filter_complex",
                "[:1]channelmap=channel_layout='5.1'",
                "-c:s",
                "copy",
                "/tmp/0/audio.mkv"
            ]
        );
    }

    #[test]
    fn builds_mkvmerge_concat_options() {
        let options = mkvmerge_options("video.new.mkv", "/tmp/0", 1, "ivf", false);

        assert_eq!(
            options,
            [
                "-o",
                "video.new.mkv",
                "/tmp/0/audio.mkv",
                "--append-to",
                "2:0:1:0",
                "/tmp/0/000000.ivf",
                "+/tmp/0/000001.ivf"
            ]
        );
    }

    #[test]
    fn appends_every_scene_to_the_previous_video_file() {
        let options = mkvmerge_options("video.new.mkv", "/tmp/0", 2, "ivf", false);

        assert_eq!(
            options,
            [
                "-o",
                "video.new.mkv",
                "/tmp/0/audio.mkv",
                "--append-to",
                "2:0:1:0,3:0:2:0",
                "/tmp/0/000000.ivf",
                "+/tmp/0/000001.ivf",
                "+/tmp/0/000002.ivf"
            ]
        );
    }

    #[test]
    fn single_scene_muxes_without_an_append_mapping() {
        let options = mkvmerge_options("video.new.mkv", "/tmp/0", 0, "ivf", false);

        assert_eq!(
            options,
            [
                "-o",
                "video.new.mkv",
                "/tmp/0/audio.mkv",
                "/tmp/0/000000.ivf"
            ]
        );
    }

    #[test]
    fn places_timestamps_before_the_x265_hevc_file_list() {
        let options = mkvmerge_options("video.new.mkv", "/tmp/0", 1, "hevc", true);

        assert_eq!(
            options,
            [
                "-o",
                "video.new.mkv",
                "/tmp/0/audio.mkv",
                "--append-to",
                "2:0:1:0",
                "--timestamps",
                "0:/tmp/0/timecodes.txt",
                "/tmp/0/000000.hevc",
                "+/tmp/0/000001.hevc"
            ]
        );
    }

    #[test]
    fn writes_scenes_as_ffv1_matroska() {
        let command = ffv1_scene_command("/tmp/0/000000.mkv");
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect();

        assert_eq!(
            args,
            [
                "-nostdin",
                "-y",
                "-loglevel",
                "error",
                "-f",
                "yuv4mpegpipe",
                "-i",
                "pipe:0",
                "-c:v",
                "ffv1",
                "-f",
                "matroska",
                "/tmp/0/000000.mkv",
            ]
        );
    }

    #[test]
    fn decodes_scene_ffv1_to_y4m_for_encoders() {
        let command = ffv1_decoder_command("/tmp/0/000000.mkv");
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect();

        assert_eq!(
            args,
            [
                "-nostdin",
                "-loglevel",
                "error",
                "-threads",
                "4",
                "-i",
                "/tmp/0/000000.mkv",
                "-strict",
                "-1",
                "-f",
                "yuv4mpegpipe",
                "pipe:1",
            ]
        );
    }

    #[test]
    fn rejects_invalid_numeric_option() {
        let result = cli().try_get_matches_from([
            "sav1n",
            "-i",
            "input.mkv",
            "-v",
            "script.vpy",
            "-e",
            "many",
        ]);

        assert!(result.is_err());
    }
}

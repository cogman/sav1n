mod aom_firstpass;
mod av1_encoder;
mod cq_search;
mod encoder;
mod frame;
mod frame_buffer;
mod metric;
mod shutdown;
mod video_header;
mod vp9_encoder;
mod x265_encoder;

use crate::aom_firstpass::aom::AomFirstpass;
use crate::frame::Frame;
use crate::frame::Status::Processing;
use crate::frame_buffer::FrameBuffer;
use crate::metric::{MetricConfig, MetricKind, PreparedReference};
use crate::shutdown::TrackedChild;
use crate::video_header::VideoHeader;
use clap::{value_parser, Arg, ArgMatches, Command as ClapCommand};
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
use std::sync::Arc;
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
    let trial_cpu_used = *options.get_one::<u32>("trial_cpu_used").unwrap();
    let vpy: String = options.get_one::<String>("vpy").unwrap().clone();
    let metric = Arc::new(metric_options(&options));
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
        let metric = metric.clone();

        tasks.push(tokio::spawn(async move {
            compress_file(
                cpu_used,
                trial_cpu_used,
                vpy,
                metric,
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
    trial_cpu_used: u32,
    vpy: String,
    metric: Arc<MetricConfig>,
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
        metric,
        cpu_used,
        trial_cpu_used,
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
    let audio_streams = streams
        .iter()
        .filter(|stream| stream["codec_type"] == "audio");
    for (audio_index, stream) in audio_streams.enumerate() {
        let channels = stream["channels"].as_u64().unwrap();
        let bitrate = channels * 42;
        audio_encode
            .arg(format!("-b:a:{}", audio_index))
            .arg(format!("{}K", bitrate));
        if let Some(layout) = stream["channel_layout"].as_str() {
            let channel_layout = layout.strip_suffix("(side)").unwrap_or(layout);
            audio_encode
                .arg(format!("-filter:a:{}", audio_index))
                .arg(format!("channelmap=channel_layout='{}'", channel_layout));
        }
    }
    audio_encode
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
    metric: Arc<MetricConfig>,
    cpu_used: u32,
    trial_cpu_used: u32,
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
                        metric.clone(),
                        cpu_used,
                        trial_cpu_used,
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
                metric,
                cpu_used,
                trial_cpu_used,
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

// These overrides are scoped to the test task, not the process environment.
#[cfg(test)]
tokio::task_local! {
    static TEST_PIPELINE_ENV: (PathBuf, String);
}

#[cfg(test)]
fn configure_test_pipeline_command(command: &mut Command) {
    let _ = TEST_PIPELINE_ENV.try_with(|(path, root)| {
        command.env("PATH", path).env("MOCK_SCENARIO", root);
    });
}

// Tokio task locals are not inherited by spawned tasks. Capture both command
// environments before spawning and restore them while the child future runs.
#[cfg(test)]
fn inherit_test_command_env<F>(future: F) -> impl std::future::Future<Output = F::Output> + Send
where
    F: std::future::Future + Send,
{
    let pipeline = TEST_PIPELINE_ENV.try_with(Clone::clone).ok();
    let metric = metric::TEST_METRIC_ENV.try_with(Clone::clone).ok();
    async move {
        let future = async move {
            if let Some(metric) = metric {
                metric::TEST_METRIC_ENV.scope(metric, future).await
            } else {
                future.await
            }
        };
        if let Some(pipeline) = pipeline {
            TEST_PIPELINE_ENV.scope(pipeline, future).await
        } else {
            future.await
        }
    }
}

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
    #[cfg(test)]
    configure_test_pipeline_command(&mut command);
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
    #[cfg(test)]
    configure_test_pipeline_command(&mut command);
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
    metric: Arc<MetricConfig>,
    cpu_used: u32,
    trial_cpu_used: u32,
    tmp_folder: String,
    encoder: Arc<dyn Encoder + Send + Sync>,
) -> JoinHandle<()> {
    let mut permits = encoding_scenes.acquire_many_owned(2).await.unwrap();
    let scene = async move {
        scene_file.wait().await;
        let first_pass = async {
            if let Some(mut first_pass) =
                first_pass(scene_number, tmp_folder.clone(), encoder.clone()).await
            {
                first_pass.wait().await;
            }
        };
        let (_, reference) = join!(
            first_pass,
            prepare_reference(scene_number, &tmp_folder, &metric)
        );
        let reference = Arc::new(reference);
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
        let (initial_min_score, initial_max_score) = join!(
            evaluate_trial(
                scene_number,
                initial_min,
                6,
                1,
                tmp_folder.clone(),
                encoder.clone(),
                metric.clone(),
                reference.clone(),
            ),
            evaluate_trial(
                scene_number,
                initial_max,
                6,
                1,
                tmp_folder.clone(),
                encoder.clone(),
                metric.clone(),
                reference.clone(),
            ),
        );

        let config = cq_search::SearchConfig {
            min: 10,
            max: encoder.max_cq(),
            target: metric.search_target(),
            tolerance: metric.search_tolerance(),
            max_new_evaluations: 10,
        };
        let search_tmp_folder = tmp_folder.clone();
        let search_encoder = encoder.clone();
        let search_metric = metric.clone();
        let search_reference = reference.clone();
        let outcome = cq_search::search(
            &config,
            &[
                cq_search::Sample {
                    cq: initial_min,
                    score: initial_min_score,
                },
                cq_search::Sample {
                    cq: initial_max,
                    score: initial_max_score,
                },
            ],
            move |cq| {
                let tmp_folder = search_tmp_folder.clone();
                let encoder = search_encoder.clone();
                let metric = search_metric.clone();
                let reference = search_reference.clone();
                async move {
                    evaluate_trial(
                        scene_number,
                        cq,
                        trial_cpu_used,
                        2,
                        tmp_folder,
                        encoder,
                        metric,
                        reference,
                    )
                    .await
                }
            },
            |sample| {
                // The Butteraugli sign conversion is its own inverse.
                let native_score = metric.kind.search_score(sample.score);
                println!(
                    "{}: CQ {}: {} {:.4} (target {:.4})",
                    scene_number,
                    sample.cq,
                    metric.kind.name(),
                    native_score,
                    metric.target
                );
            },
        )
        .await;
        let target_met = outcome
            .samples
            .iter()
            .any(|sample| sample.cq == outcome.cq && sample.score >= config.target);
        println!(
            "{}: chose {} ({} samples, converged: {}, target met: {})",
            scene_number,
            outcome.cq,
            outcome.samples.len(),
            outcome.converged,
            target_met
        );
        let cq = outcome.cq;
        drop(permits.split(1));
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
        drop(permits);
        cleanup(scene_number, tmp_folder).await;
    };
    #[cfg(test)]
    let scene = inherit_test_command_env(scene);
    tokio::spawn(scene)
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
    let remove_metric = remove_dir_all(format!("{}.metric", scene_str));
    let (video, scene, metric) = join!(remove_video, remove_scene, remove_metric);
    video.unwrap();
    for (result, name) in [(scene, "log"), (metric, "metric")] {
        if let Err(e) = result {
            if e.kind() != ErrorKind::NotFound {
                panic!("Failed to remove {}.{}: {}", scene_str, name, e);
            }
        }
    }
}

/// Probe the scene reference once; trials share it and the `.metric` directory.
async fn prepare_reference(
    scene_number: u32,
    tmp_folder: &str,
    metric: &MetricConfig,
) -> PreparedReference {
    let scene_str = format!("{}/{:06}", tmp_folder, scene_number);
    let work_dir = PathBuf::from(format!("{}.metric", scene_str));
    match metric
        .prepare_reference(&format!("{}.mkv", scene_str), &work_dir)
        .await
    {
        Ok(reference) => reference,
        Err(error) => {
            let _ = remove_dir_all(&work_dir).await;
            panic!(
                "{} reference preparation failed for scene {}: {}",
                metric.kind.name(),
                scene_number,
                error
            )
        }
    }
}

/// Encode once, then let the selected metric prepare and compare its inputs.
/// Each probe owns its directory, including the two probes run concurrently.
async fn evaluate_trial(
    scene_number: u32,
    cq: u32,
    cpu_used: u32,
    threads: u32,
    tmp_folder: String,
    encoder: Arc<dyn Encoder + Send + Sync>,
    metric: Arc<MetricConfig>,
    reference: Arc<PreparedReference>,
) -> f64 {
    let scene_str = format!("{}/{:06}", tmp_folder, scene_number);
    let scene_path = format!("{}.mkv", scene_str);
    let work_dir = PathBuf::from(format!("{}.probe-{}", scene_str, cq));
    create_dir(&work_dir)
        .await
        .expect("failed to create metric workspace");
    let trial_path = work_dir.join(format!("trial.{}", encoder.container_format()));
    let result: Result<f64, String> = async {
        let mut command = encoder.second_pass(EncoderOptions {
            cq,
            cpu_used,
            threads,
            log_file: format!("{}.log", scene_str).as_str(),
            output: trial_path.to_str().unwrap(),
            input: "-",
            ..Default::default()
        });
        let mut decoder = TrackedChild::spawn(ffv1_decoder_command(&scene_path))
            .map_err(|error| format!("failed to start trial reference decoder: {error}"))?;
        let decoder_stdout: Stdio = decoder
            .stdout
            .take()
            .expect("reference decoder stdout is piped")
            .try_into()
            .map_err(|error| format!("failed to pipe reference decoder: {error}"))?;
        command.stdin(decoder_stdout);
        let mut encode = TrackedChild::spawn(command)
            .map_err(|error| format!("failed to start trial encoder: {error}"))?;
        let (encode_status, decoder_status) = join!(encode.wait(), decoder.wait());
        let encode_status = encode_status.map_err(|e| e.to_string())?;
        let decoder_status = decoder_status.map_err(|e| e.to_string())?;
        if !encode_status.success() {
            return Err(format!("trial encoder failed ({encode_status})"));
        }
        if !decoder_status.success() {
            return Err(format!("trial reference decoder failed ({decoder_status})"));
        }
        metric
            .evaluate(&reference, trial_path.to_str().unwrap(), &work_dir)
            .await
    }
    .await;
    let cleanup = remove_dir_all(&work_dir).await;
    let score = result.unwrap_or_else(|error| {
        panic!(
            "{} evaluation failed for scene {} CQ {}: {}",
            metric.kind.name(),
            scene_number,
            cq,
            error
        )
    });
    cleanup.expect("failed to remove metric workspace");
    assert!(score.is_finite(), "metric returned a non-finite score");
    metric.kind.search_score(score)
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
            Arg::new("metric")
                .long("metric")
                .help("Quality metric used for per-scene CQ search")
                .default_value("vmaf")
                .value_parser(["vmaf", "cvvdp", "butteraugli"]),
        )
        .arg(
            Arg::new("target")
                .short('t')
                .long("target")
                .visible_alias("vmaf_target")
                .help("Target in native metric units (VMAF defaults to 95)")
                .default_value_if("metric", "vmaf", "95")
                .required_if_eq("metric", "cvvdp")
                .required_if_eq("metric", "butteraugli")
                .allow_negative_numbers(true)
                .value_parser(finite_score),
        )
        .arg(
            Arg::new("tolerance")
                .long("tolerance")
                .help("Acceptable distance from target; 0 fully resolves the CQ bracket")
                .default_value_if("metric", "vmaf", "0.5")
                .default_value_if("metric", "cvvdp", "0.05")
                .default_value_if("metric", "butteraugli", "0.05")
                .value_parser(nonnegative_score),
        )
        .arg(
            Arg::new("cvvdp_display")
                .long("cvvdp-display")
                .help("CVVDP display model; select a matching model for HDR content")
                .default_value("standard_fhd"),
        )
        .arg(
            Arg::new("cvvdp_device")
                .long("cvvdp-device")
                .help("CVVDP compute device: cpu or a GPU index (e.g. 0)")
                .default_value("cpu")
                .value_parser(cvvdp_device),
        )
        .arg(
            Arg::new("cpu_used")
                .short('c')
                .long("cpu_used")
                .default_value("0")
                .value_parser(value_parser!(u32)),
        )
        .arg(
            Arg::new("trial_cpu_used")
                .long("trial-cpu-used")
                .visible_alias("vmaf_cpu_used")
                .help("Encoder speed for quality-search trials, not metric compute settings")
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

fn finite_score(value: &str) -> Result<f64, String> {
    let score = value.parse::<f64>().map_err(|e| e.to_string())?;
    if !score.is_finite() {
        return Err("score must be finite".into());
    }
    Ok(score)
}

fn nonnegative_score(value: &str) -> Result<f64, String> {
    let score = finite_score(value)?;
    if score < 0.0 {
        return Err("value must be nonnegative".into());
    }
    Ok(score)
}

fn cvvdp_device(value: &str) -> Result<String, String> {
    if value == "cpu" || value.parse::<u32>().is_ok() {
        Ok(value.to_string())
    } else {
        Err("device must be cpu or a nonnegative GPU index".into())
    }
}

fn metric_options(options: &ArgMatches) -> MetricConfig {
    let kind = match options.get_one::<String>("metric").unwrap().as_str() {
        "vmaf" => MetricKind::Vmaf,
        "cvvdp" => MetricKind::Cvvdp,
        "butteraugli" => MetricKind::Butteraugli,
        _ => unreachable!("validated by clap"),
    };
    MetricConfig {
        kind,
        target: *options.get_one::<f64>("target").unwrap(),
        tolerance: *options.get_one::<f64>("tolerance").unwrap(),
        display: options.get_one::<String>("cvvdp_display").unwrap().clone(),
        device: options.get_one::<String>("cvvdp_device").unwrap().clone(),
    }
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

    #[cfg(unix)]
    mod mock_pipeline {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        struct TestDir(PathBuf);

        impl Drop for TestDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        fn executable(root: &Path, name: &str, script: &str) {
            let path = root.join(name);
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        // Run in-process so coverage tools see the production paths, while
        // command environments remain scoped to this task and its children.
        #[tokio::test]
        async fn scene_pipeline_with_mock_commands() {
            let root = TestDir(
                std::env::temp_dir().join(format!("sav1n mock pipeline {}", std::process::id())),
            );
            std::fs::create_dir(&root.0).unwrap();
            executable(
                &root.0,
                "ffprobe",
                r##"#!/bin/sh
printf 'probe %s\n' "$*" >> "$MOCK_SCENARIO/calls"
for last do :; done
[ -f "$last" ] || exit 8
[ ! -f "$MOCK_SCENARIO/fail-probe" ] || exit 9
printf '%s\n' '{"streams":[{"width":64,"height":48,"nb_read_frames":"1","nb_read_packets":"1","avg_frame_rate":"24/1","pix_fmt":"yuv420p","color_primaries":"bt709","color_transfer":"bt709","color_space":"bt709","color_range":"tv"}]}'
"##,
            );
            executable(
                &root.0,
                "ffmpeg",
                r##"#!/bin/sh
printf 'ffmpeg %s\n' "$*" >> "$MOCK_SCENARIO/calls"
for last do :; done
case " $* " in
  *' -lavfi '*)
    [ ! -f "$MOCK_SCENARIO/fail-vmaf" ] || exit 10
    for arg do case "$arg" in *.probe-*/trial.ivf) trial=$arg;; esac; done
    probe=${trial%/*}; cq=${probe##*.probe-}
    printf 'VMAF score: %s\n' "$((100 - cq))" >&2
    ;;
  *' -f image2pipe '*) /bin/cat "$MOCK_SCENARIO/frame.png" ;;
  *' -fps_mode passthrough '*)
    [ ! -f "$MOCK_SCENARIO/fail-comparison" ] || exit 11
    : > "$last"
    ;;
  *' -f matroska '*) /bin/cat > "$last" ;;
  *)
    printf 'mock y4m\n'
    [ ! -f "$MOCK_SCENARIO/fail-decoder" ] || exit 12
    ;;
esac
"##,
            );
            executable(
                &root.0,
                "mock-encoder",
                r##"#!/bin/sh
printf 'encoder %s %s %s %s %s %s\n' "$1" "$2" "$3" "$4" "$5" "$6" >> "$MOCK_SCENARIO/calls"
/bin/cat > /dev/null
[ "$7" != fail ] || exit 13
if [ "$1" = first ]; then printf 'pass log' > "$6"; else printf 'cq=%s' "$2" > "$5"; fi
"##,
            );
            executable(
                &root.0,
                "cvvdp",
                r##"#!/bin/sh
printf 'cvvdp %s\n' "$*" >> "$MOCK_SCENARIO/calls"
while [ "$#" -gt 0 ]; do
  if [ "$1" = --result ]; then shift; result=$1; fi
  shift
done
printf 'test,reference,CVVDP\ntest,ref,8.25\n' > "$result"
"##,
            );
            executable(
                &root.0,
                "butteraugli_main",
                r##"#!/bin/sh
printf 'butteraugli %s\n' "$*" >> "$MOCK_SCENARIO/calls"
printf '1.25\n3-norm: 0.5\n'
"##,
            );

            // A minimal RGB16 PNG stream; the mock scorer only reads the files.
            let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
            png.extend_from_slice(&13u32.to_be_bytes());
            png.extend_from_slice(b"IHDR");
            png.extend_from_slice(&64u32.to_be_bytes());
            png.extend_from_slice(&48u32.to_be_bytes());
            png.extend_from_slice(&[16, 2, 0, 0, 0]);
            png.extend_from_slice(&[0; 4]);
            png.extend_from_slice(&0u32.to_be_bytes());
            png.extend_from_slice(b"IEND");
            png.extend_from_slice(&[0; 4]);
            std::fs::write(root.0.join("frame.png"), png).unwrap();

            let environment = (root.0.clone(), root.0.to_str().unwrap().to_string());
            TEST_PIPELINE_ENV
                .scope(
                    environment.clone(),
                    metric::TEST_METRIC_ENV.scope(environment, exercise_pipeline(root.0.clone())),
                )
                .await;
        }

        struct MockEncoder {
            first_pass: bool,
            fail: bool,
        }

        impl MockEncoder {
            fn command(&self, phase: &str, options: EncoderOptions<'_>) -> Command {
                let mut command = Command::new("mock-encoder");
                command.args([
                    phase,
                    &options.cq.to_string(),
                    &options.cpu_used.to_string(),
                    &options.threads.to_string(),
                    options.output,
                    options.log_file,
                    if self.fail { "fail" } else { "ok" },
                ]);
                configure_test_pipeline_command(&mut command);
                command
            }
        }

        impl Encoder for MockEncoder {
            fn first_pass(&self, options: EncoderOptions<'_>) -> Option<Command> {
                self.first_pass.then(|| self.command("first", options))
            }

            fn second_pass(&self, options: EncoderOptions<'_>) -> Command {
                self.command("second", options)
            }

            fn container_format(&self) -> &'static str {
                "ivf"
            }
            fn max_cq(&self) -> u32 {
                50
            }
        }

        fn metric(kind: MetricKind) -> Arc<MetricConfig> {
            Arc::new(MetricConfig {
                kind,
                target: 75.0,
                tolerance: 0.0,
                display: "standard_fhd".into(),
                device: "cpu".into(),
            })
        }

        fn panic_message(error: tokio::task::JoinError) -> String {
            let panic = error.into_panic();
            if let Some(message) = panic.downcast_ref::<String>() {
                message.clone()
            } else {
                panic.downcast_ref::<&str>().unwrap().to_string()
            }
        }

        async fn exercise_pipeline(root: PathBuf) {
            let dir = root.to_str().unwrap().to_string();
            let encoder: Arc<dyn Encoder + Send + Sync> = Arc::new(MockEncoder {
                first_pass: true,
                fail: false,
            });
            let header = Arc::new(VideoHeader {
                as_bytes: b"YUV4MPEG2 W64 H48 F24:1 C420\n".to_vec(),
                ..VideoHeader::new()
            });
            let writer = create_scene_file(&dir, 1, header.clone()).await;
            let writer = write_scene_frame(writer, Arc::new(Frame::new(8, 0))).await;
            let permits = Arc::new(Semaphore::new(2));
            let prior = Arc::new(Mutex::new(vec![10, 12, 14, 16, 18, 20, 22, 24, 26, 28]));
            compress_scene(
                1,
                writer.close(),
                permits.clone(),
                prior.clone(),
                metric(MetricKind::Vmaf),
                2,
                4,
                dir.clone(),
                encoder.clone(),
            )
            .await
            .await
            .unwrap();

            assert_eq!(permits.available_permits(), 2);
            assert_eq!(
                *prior.lock().await,
                [10, 12, 14, 16, 18, 20, 22, 24, 25, 26, 28]
            );
            assert_eq!(
                std::fs::read_to_string(root.join("000001.ivf")).unwrap(),
                "cq=25"
            );
            for path in [
                "000001.mkv",
                "000001.log",
                "000001.metric",
                "000001.probe-12",
                "000001.probe-26",
            ] {
                assert!(!root.join(path).exists(), "{path} was not cleaned up");
            }
            let log = std::fs::read_to_string(root.join("calls")).unwrap();
            assert!(log.contains("encoder first 20 0 1 /dev/null"));
            assert!(log.contains("encoder second 12 6 1 "));
            assert!(log.contains("encoder second 26 6 1 "));
            assert!(log.contains("encoder second 25 2 1 "));
            assert!(log
                .lines()
                .any(|line| line.starts_with("encoder second ") && line.contains(" 4 2 ")));
            assert_eq!(
                log.lines()
                    .filter(|line| line.starts_with("probe ")
                        && line.contains("-count_frames")
                        && line.contains("000001.mkv"))
                    .count(),
                1
            );

            // Reference preparation is shared; two probes have separate workspaces.
            std::fs::write(root.join("000002.mkv"), "reference").unwrap();
            let cvvdp = metric(MetricKind::Cvvdp);
            let reference = Arc::new(prepare_reference(2, &dir, &cvvdp).await);
            assert!(root.join("000002.metric/cvvdp-reference.mkv").exists());
            let (a, b) = tokio::join!(
                evaluate_trial(
                    2,
                    21,
                    3,
                    2,
                    dir.clone(),
                    encoder.clone(),
                    cvvdp.clone(),
                    reference.clone()
                ),
                evaluate_trial(
                    2,
                    22,
                    3,
                    2,
                    dir.clone(),
                    encoder.clone(),
                    cvvdp.clone(),
                    reference.clone()
                ),
            );
            assert_eq!((a, b), (8.25, 8.25));
            assert!(!root.join("000002.probe-21").exists());
            assert!(!root.join("000002.probe-22").exists());
            cleanup(2, dir.clone()).await;
            assert!(!root.join("000002.metric").exists());
            assert!(!root.join("000002.mkv").exists());

            // Lower-is-better distances are negated when returned to CQ search.
            std::fs::write(root.join("000003.mkv"), "reference").unwrap();
            let butter = metric(MetricKind::Butteraugli);
            let reference = Arc::new(prepare_reference(3, &dir, &butter).await);
            let score =
                evaluate_trial(3, 30, 3, 2, dir.clone(), encoder.clone(), butter, reference).await;
            assert_eq!(score, -1.25);
            assert!(!root.join("000003.probe-30").exists());
            cleanup(3, dir.clone()).await;

            // Preparation failure removes its partially created shared workspace.
            std::fs::write(root.join("000004.mkv"), "reference").unwrap();
            std::fs::write(root.join("fail-comparison"), "").unwrap();
            let child_dir = dir.clone();
            let error = tokio::spawn(inherit_test_command_env(async move {
                prepare_reference(4, &child_dir, &metric(MetricKind::Cvvdp)).await
            }))
            .await
            .unwrap_err();
            let message = panic_message(error);
            assert!(
                message.contains("cvvdp reference preparation failed for scene 4"),
                "{message}"
            );
            assert!(!root.join("000004.metric").exists());
            std::fs::remove_file(root.join("fail-comparison")).unwrap();

            // Trial failures identify the scene and CQ and still remove workspaces.
            let vmaf = metric(MetricKind::Vmaf);
            let reference = Arc::new(prepare_reference(4, &dir, &vmaf).await);
            for (flag, failing_encoder, expected) in [
                (None, true, "trial encoder failed"),
                (
                    Some("fail-decoder"),
                    false,
                    "trial reference decoder failed",
                ),
                (Some("fail-vmaf"), false, "FFmpeg libvmaf"),
            ] {
                if let Some(flag) = flag {
                    std::fs::write(root.join(flag), "").unwrap();
                }
                let child_dir = dir.clone();
                let child_reference = reference.clone();
                let child_metric = vmaf.clone();
                let child_encoder: Arc<dyn Encoder + Send + Sync> = Arc::new(MockEncoder {
                    first_pass: false,
                    fail: failing_encoder,
                });
                let error = tokio::spawn(inherit_test_command_env(async move {
                    evaluate_trial(
                        4,
                        31,
                        3,
                        2,
                        child_dir,
                        child_encoder,
                        child_metric,
                        child_reference,
                    )
                    .await
                }))
                .await
                .unwrap_err();
                let message = panic_message(error);
                assert!(
                    message.contains("vmaf evaluation failed for scene 4 CQ 31"),
                    "{message}"
                );
                assert!(message.contains(expected), "{message}");
                assert!(!root.join("000004.probe-31").exists());
                if let Some(flag) = flag {
                    std::fs::remove_file(root.join(flag)).unwrap();
                }
            }
            cleanup(4, dir.clone()).await;

            // A failed scene must release both permits rather than block later scenes.
            std::fs::write(root.join("fail-vmaf"), "").unwrap();
            let writer = create_scene_file(&dir, 5, header).await;
            let writer = write_scene_frame(writer, Arc::new(Frame::new(8, 0))).await;
            let error = compress_scene(
                5,
                writer.close(),
                permits.clone(),
                Arc::new(Mutex::new(Vec::new())),
                metric(MetricKind::Vmaf),
                2,
                4,
                dir.clone(),
                encoder,
            )
            .await
            .await
            .unwrap_err();
            assert!(panic_message(error).contains("vmaf evaluation failed for scene 5"));
            assert_eq!(permits.available_permits(), 2);
            std::fs::remove_file(root.join("fail-vmaf")).unwrap();
            cleanup(5, dir).await;
        }
    }

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
        assert_eq!(*matches.get_one::<f64>("target").unwrap(), 95.0);
        assert_eq!(*matches.get_one::<u32>("cpu_used").unwrap(), 0);
        assert_eq!(*matches.get_one::<u32>("trial_cpu_used").unwrap(), 3);
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
        assert_eq!(*matches.get_one::<f64>("target").unwrap(), 92.0);
        assert_eq!(*matches.get_one::<u32>("cpu_used").unwrap(), 2);
        assert_eq!(*matches.get_one::<u32>("trial_cpu_used").unwrap(), 5);
        assert_eq!(matches.get_one::<String>("codec").unwrap(), "x265");
    }

    #[test]
    fn parses_metric_targets_in_native_units() {
        for (name, kind, target, tolerance) in [
            ("vmaf", MetricKind::Vmaf, "95", 0.5),
            ("cvvdp", MetricKind::Cvvdp, "8", 0.05),
            ("butteraugli", MetricKind::Butteraugli, "1.5", 0.05),
        ] {
            let matches = cli()
                .try_get_matches_from([
                    "sav1n",
                    "-i",
                    "input.mkv",
                    "-v",
                    "script.vpy",
                    "--metric",
                    name,
                    "--target",
                    target,
                    "--trial-cpu-used",
                    "4",
                ])
                .unwrap();
            let config = metric_options(&matches);
            assert_eq!(config.kind, kind);
            assert_eq!(config.target, target.parse::<f64>().unwrap());
            assert_eq!(config.tolerance, tolerance);
            assert_eq!(*matches.get_one::<u32>("trial_cpu_used").unwrap(), 4);
        }
    }

    #[tokio::test]
    async fn search_handles_native_quality_and_distance_targets() {
        for (kind, target) in [
            (MetricKind::Vmaf, 70.0),
            (MetricKind::Cvvdp, 7.0),
            (MetricKind::Butteraugli, 3.0),
        ] {
            let config = cq_search::SearchConfig {
                min: 10,
                max: 50,
                target: kind.search_score(target),
                tolerance: 0.0,
                max_new_evaluations: 20,
            };
            let result = cq_search::search(
                &config,
                &[],
                move |cq| async move {
                    let native = match kind {
                        MetricKind::Vmaf => 100.0 - cq as f64,
                        MetricKind::Cvvdp => 10.0 - cq as f64 / 10.0,
                        MetricKind::Butteraugli => cq as f64 / 10.0,
                    };
                    kind.search_score(native)
                },
                |_| {},
            )
            .await;
            assert_eq!(result.cq, 30, "{kind:?}");
        }
    }

    #[test]
    fn alternative_metrics_require_explicit_targets() {
        for metric in ["cvvdp", "butteraugli", "unknown"] {
            assert!(cli()
                .try_get_matches_from([
                    "sav1n",
                    "-i",
                    "input.mkv",
                    "-v",
                    "script.vpy",
                    "--metric",
                    metric,
                ])
                .is_err());
        }
    }

    #[test]
    fn accepts_legacy_target_alias_and_custom_metric_settings() {
        let matches = cli()
            .try_get_matches_from([
                "sav1n",
                "-i",
                "input.mkv",
                "-v",
                "script.vpy",
                "--vmaf_target",
                "93",
                "--tolerance",
                "0",
                "--cvvdp-display",
                "standard_4k",
                "--cvvdp-device",
                "cpu",
            ])
            .unwrap();
        let config = metric_options(&matches);
        assert_eq!(config.target, 93.0);
        assert_eq!(config.tolerance, 0.0);
        assert_eq!(config.display, "standard_4k");
        assert_eq!(config.device, "cpu");
    }

    #[test]
    fn rejects_nonfinite_scores_and_negative_tolerances() {
        for (flag, value) in [
            ("--target", "NaN"),
            ("--target", "inf"),
            ("--target", "-inf"),
            ("--tolerance", "NaN"),
            ("--tolerance", "-0.1"),
            ("--cvvdp-device", "cuda"),
        ] {
            assert!(cli()
                .try_get_matches_from(
                    ["sav1n", "-i", "input.mkv", "-v", "script.vpy", flag, value,]
                )
                .is_err());
        }
    }

    #[test]
    fn metric_defaults_and_overrides_reach_the_configuration() {
        let defaults = cli()
            .try_get_matches_from(["sav1n", "-i", "input.mkv", "-v", "script.vpy"])
            .unwrap();
        let config = metric_options(&defaults);
        assert_eq!(config.kind, MetricKind::Vmaf);
        assert_eq!(config.target, 95.0);
        assert_eq!(config.tolerance, 0.5);
        assert_eq!(config.display, "standard_fhd");
        assert_eq!(config.device, "cpu");

        for metric in ["cvvdp", "butteraugli"] {
            let matches = cli()
                .try_get_matches_from([
                    "sav1n",
                    "-i",
                    "input.mkv",
                    "-v",
                    "script.vpy",
                    "--metric",
                    metric,
                    "--target",
                    "-1.25",
                    "--tolerance",
                    "0.125",
                    "--cvvdp-display",
                    "hdr_pq",
                    "--cvvdp-device",
                    "2",
                ])
                .unwrap();
            let config = metric_options(&matches);
            assert_eq!(config.target, -1.25);
            assert_eq!(config.tolerance, 0.125);
            assert_eq!(config.display, "hdr_pq");
            assert_eq!(config.device, "2");
        }
    }

    #[test]
    fn cli_reports_missing_required_arguments_and_metric_targets() {
        use clap::error::ErrorKind;

        for args in [
            vec!["sav1n"],
            vec!["sav1n", "-i", "input.mkv"],
            vec!["sav1n", "-v", "script.vpy"],
            vec![
                "sav1n",
                "-i",
                "input.mkv",
                "-v",
                "script.vpy",
                "--metric",
                "cvvdp",
            ],
            vec![
                "sav1n",
                "-i",
                "input.mkv",
                "-v",
                "script.vpy",
                "--metric",
                "butteraugli",
            ],
        ] {
            assert_eq!(
                cli().try_get_matches_from(args).unwrap_err().kind(),
                ErrorKind::MissingRequiredArgument
            );
        }
    }

    #[test]
    fn cli_rejects_invalid_metric_values_and_devices() {
        for (flag, value) in [
            ("--metric", "unknown"),
            ("--target", "not-a-number"),
            ("--target", "1e999"),
            ("--tolerance", "inf"),
            ("--tolerance", "not-a-number"),
            ("--cvvdp-device", "-1"),
            ("--cvvdp-device", "1.5"),
            ("--cvvdp-device", "4294967296"),
        ] {
            let args = ["sav1n", "-i", "input.mkv", "-v", "script.vpy", flag, value];
            assert!(cli().try_get_matches_from(args).is_err(), "{flag} {value}");
        }
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
                "-filter:a:0",
                "channelmap=channel_layout='5.1'",
                "-c:s",
                "copy",
                "/tmp/0/audio.mkv"
            ]
        );
    }

    #[test]
    fn audio_encode_maps_channels_per_output_audio_stream() {
        let streams = [
            serde_json::json!({"codec_type": "video"}),
            serde_json::json!({"codec_type": "audio", "channel_layout": "5.1(side)", "channels": 6}),
            serde_json::json!({"codec_type": "subtitle"}),
            serde_json::json!({"codec_type": "audio", "channel_layout": "stereo", "channels": 2}),
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
                "-filter:a:0",
                "channelmap=channel_layout='5.1'",
                "-b:a:1",
                "84K",
                "-filter:a:1",
                "channelmap=channel_layout='stereo'",
                "-c:s",
                "copy",
                "/tmp/0/audio.mkv"
            ]
        );
    }

    #[test]
    fn audio_encode_skips_channel_map_for_unknown_layout() {
        let streams = [serde_json::json!({"codec_type": "audio", "channels": 6})];

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

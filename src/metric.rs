//! Native-unit quality metrics. All inputs are compared by decoded frame index.
//!
//! Work that depends only on the reference (a decoding frame count, CVVDP's
//! retimed copy, Butteraugli's color validation) runs once per scene in
//! `prepare_reference`; each trial then only demuxes to count its packets.
//!
//! Butteraugli supports BT.709/sRGB SDR only (no tone mapping). Missing reference
//! tags mean BT.709 limited-range YUV, or full-range sRGB for RGB pixels; missing
//! trial tags inherit the reference tags. Explicit incompatible tags are rejected.
//! Two FFmpeg streams emit uncompressed RGB16 PNGs and the next frame pair is
//! decoded while the current pair is scored: disk storage is two PNGs, not two
//! entire decoded videos. FFmpeg needs zscale.
//! CVVDP uses two lossless FFV1 comparison videos, retimed to reference FPS
//! without frame duplication/dropping; these consume disk proportional to scene
//! length and are removed by the caller with the work directories. The official
//! v0.5.7 CLI uses --device and --result (not --gpu). See upstream
//! pycvvdp/run_cvvdp.py and libjxl/tools/butteraugli_main.cc for result formats.

use crate::shutdown::TrackedChild;
use serde_json::Value;
use std::path::Path;
use std::process::{Output, Stdio};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{ChildStdout, Command};
use tokio::task::JoinHandle;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MetricKind {
    Vmaf,
    Cvvdp,
    Butteraugli,
}

impl MetricKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Vmaf => "vmaf",
            Self::Cvvdp => "cvvdp",
            Self::Butteraugli => "butteraugli",
        }
    }

    /// Search always maximizes quality; reporting retains the native units.
    pub fn search_score(self, native: f64) -> f64 {
        match self {
            Self::Butteraugli => -native,
            _ => native,
        }
    }
}

#[derive(Clone, Debug)]
pub struct MetricConfig {
    pub kind: MetricKind,
    pub target: f64,
    pub tolerance: f64,
    pub display: String,
    pub device: String,
}

/// Reference facts and metric inputs shared by every trial of one scene.
#[derive(Debug)]
pub struct PreparedReference {
    path: String,
    info: VideoInfo,
    /// CVVDP's retimed lossless reference and its container frame rate.
    cvvdp: Option<(String, f64)>,
    /// Butteraugli's validated SDR interpretation of the reference.
    color: Option<SdrColor>,
}

fn utf8_path(path: &Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| format!("metric work path {} must be UTF-8", path.display()))
}

fn cvvdp_path(path: &str) -> Result<(), String> {
    // Upstream CSV does not quote filenames. Parse only the last column;
    // embedded newlines would make that ambiguous.
    if path.contains(['\n', '\r']) {
        return Err("CVVDP input paths cannot contain newlines".into());
    }
    Ok(())
}

impl MetricConfig {
    pub fn search_target(&self) -> f64 {
        self.kind.search_score(self.target)
    }

    pub fn search_tolerance(&self) -> f64 {
        self.tolerance
    }

    fn validate(&self) -> Result<(), String> {
        if !self.target.is_finite() || !self.tolerance.is_finite() || self.tolerance < 0.0 {
            return Err(
                "metric target must be finite and tolerance must be finite and nonnegative".into(),
            );
        }
        if self.kind == MetricKind::Cvvdp {
            if self.display.is_empty() || self.device.is_empty() {
                return Err(
                    "CVVDP requires a display and device (defaults: standard_fhd, cpu)".into(),
                );
            }
            cvvdp_device(&self.device)?;
        }
        Ok(())
    }

    /// Decode and validate the reference once per scene. The caller owns and
    /// cleans work_dir, which must outlive every evaluation of this scene.
    pub async fn prepare_reference(
        &self,
        reference: &str,
        work_dir: &Path,
    ) -> Result<PreparedReference, String> {
        self.validate()?;
        let info = probe(reference, Count::Decoded).await?;
        let mut prepared = PreparedReference {
            path: reference.into(),
            info,
            cvvdp: None,
            color: None,
        };
        match self.kind {
            MetricKind::Vmaf => (),
            MetricKind::Cvvdp => {
                cvvdp_path(reference)?;
                tokio::fs::create_dir_all(work_dir).await.map_err(|e| {
                    format!("create metric work directory {}: {e}", work_dir.display())
                })?;
                let video = work_dir.join("cvvdp-reference.mkv");
                let info = &prepared.info;
                run(
                    comparison_command(reference, &video, info, info),
                    "FFmpeg lossless CVVDP reference preparation",
                )
                .await?;
                let path = utf8_path(&video)?;
                cvvdp_path(path)?;
                let retimed = probe(path, Count::Packets).await?;
                validate_pair(info, &retimed)?;
                prepared.cvvdp = Some((path.into(), retimed.fps));
            }
            MetricKind::Butteraugli => {
                prepared.color = Some(SdrColor::from_info(&prepared.info, None)?);
            }
        }
        Ok(prepared)
    }

    /// Return VMAF points, CVVDP JODs, or the arithmetic mean of per-frame
    /// Butteraugli distances. The caller owns and cleans the unique work_dir.
    pub async fn evaluate(
        &self,
        reference: &PreparedReference,
        distorted: &str,
        work_dir: &Path,
    ) -> Result<f64, String> {
        self.validate()?;
        let reference_info = &reference.info;
        let distorted_info = probe(distorted, Count::Packets).await?;
        validate_pair(reference_info, &distorted_info)?;
        tokio::fs::create_dir_all(work_dir)
            .await
            .map_err(|e| format!("create metric work directory {}: {e}", work_dir.display()))?;
        let score = match self.kind {
            MetricKind::Vmaf => {
                let output = run(
                    vmaf_command(&reference.path, distorted),
                    "FFmpeg libvmaf (requires /usr/local/share/model/vmaf_v0.6.1.json)",
                )
                .await?;
                parse_vmaf(&String::from_utf8_lossy(&output.stderr))?
            }
            MetricKind::Cvvdp => {
                let (ref_path, ref_fps) = reference
                    .cvvdp
                    .as_ref()
                    .ok_or("reference was not prepared for CVVDP")?;
                cvvdp_path(distorted)?;
                let result = work_dir.join("cvvdp-result.csv");
                // Do not accept a stale result from an earlier invocation.
                match tokio::fs::remove_file(&result).await {
                    Ok(()) => (),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                    Err(e) => return Err(format!("remove stale CVVDP result: {e}")),
                }
                let mut backend_config = self.clone();
                backend_config.device = cvvdp_device(&self.device)?;
                let test_video = work_dir.join("cvvdp-distorted.mkv");
                run(
                    comparison_command(distorted, &test_video, &distorted_info, reference_info),
                    "FFmpeg lossless CVVDP trial preparation",
                )
                .await?;
                let test_path = utf8_path(&test_video)?;
                cvvdp_path(test_path)?;
                let retimed = probe(test_path, Count::Packets).await?;
                validate_pair(reference_info, &retimed)?;
                if *ref_fps != retimed.fps {
                    return Err(
                        "CVVDP comparison containers report different frame rates after retiming"
                            .into(),
                    );
                }
                let output = run(
                    cvvdp_command(
                        &backend_config,
                        ref_path,
                        test_path,
                        &result,
                        reference_info.fps,
                    ),
                    "cvvdp (install current pycvvdp with --device/--result support)",
                )
                .await?;
                if self.device != "cpu"
                    && String::from_utf8_lossy(&output.stderr).contains("reverting to CPU")
                {
                    return Err(format!(
                        "CVVDP could not use requested GPU {}; {}",
                        self.device,
                        diagnostics(&output)
                    ));
                }
                let csv = tokio::fs::read_to_string(&result).await.map_err(|e| {
                    format!(
                        "CVVDP did not produce readable {}: {e}; {}",
                        result.display(),
                        diagnostics(&output)
                    )
                })?;
                parse_cvvdp_csv(&csv).map_err(|e| format!("{e}; {}", diagnostics(&output)))?
            }
            MetricKind::Butteraugli => {
                let ref_color = reference
                    .color
                    .as_ref()
                    .ok_or("reference was not prepared for Butteraugli")?;
                let test_color = SdrColor::from_info(&distorted_info, Some(ref_color))?;
                butteraugli(
                    &reference.path,
                    distorted,
                    work_dir,
                    reference_info,
                    ref_color,
                    &test_color,
                )
                .await?
            }
        };
        finite_score(score, self.kind.name())
    }
}

// Integration tests scope mock executables to their own async task without
// changing the process-wide environment used by other tests.
#[cfg(test)]
tokio::task_local! {
    pub(crate) static TEST_METRIC_ENV: (std::path::PathBuf, String);
}

fn captured(command: &mut Command) -> &mut Command {
    #[cfg(test)]
    let _ = TEST_METRIC_ENV.try_with(|(path, scenario)| {
        command.env("PATH", path).env("MOCK_SCENARIO", scenario);
    });
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
}

/// Threads for decoding the FFV1 reference, matching the encoder input decoder.
const REFERENCE_DECODE_THREADS: &str = "4";

pub(crate) fn vmaf_command(reference: &str, distorted: &str) -> Command {
    let mut command = Command::new("ffmpeg");
    // Keep the decoders threaded so they do not add to libvmaf's own
    // single-threaded (default n_threads=0) frame time.
    captured(&mut command).args([
        "-hide_banner",
        "-nostdin",
        "-y",
        "-xerror",
        "-i",
        distorted,
        "-threads",
        REFERENCE_DECODE_THREADS,
        "-i",
        reference,
        "-lavfi",
        // Matroska timestamps are rounded; do not match frames by timestamps.
        concat!(
            "[0:v:0]settb=AVTB,setpts=N[distorted];",
            "[1:v:0]settb=AVTB,setpts=N[reference];",
            "[distorted][reference]libvmaf=model='path=/usr/local/share/model/vmaf_v0.6.1.json'"
        ),
        "-an",
        "-f",
        "null",
        "-",
    ]);
    command
}

fn cvvdp_device(device: &str) -> Result<String, String> {
    if device == "cpu" {
        return Ok("cpu".into());
    }
    if !device.is_empty() && device.bytes().all(|b| b.is_ascii_digit()) {
        let index = device
            .parse::<u32>()
            .map_err(|_| "CVVDP GPU index is too large")?;
        return Ok(format!("cuda:{index}"));
    }
    Err(format!("invalid CVVDP device {device:?}: use cpu or a nonnegative GPU index (e.g. 0); official cvvdp v0.5.7 accepts --device cpu/cuda:N, not --gpu"))
}

/// Preserve decoded samples, replacing only timing. Input -r supplies reference
/// timing; passthrough (no output -r or fps filter) prevents duplicate/drop.
/// Strict +pix_fmt refuses implicit
/// format conversion if FFV1 cannot losslessly represent an input format.
fn comparison_command(
    input: &str,
    output: &Path,
    info: &VideoInfo,
    reference: &VideoInfo,
) -> Command {
    let mut command = Command::new("ffmpeg");
    let fps = reference.fps.to_string();
    let mut filter = format!("settb=AVTB,setpts=N/({fps}*TB)");
    // setparams changes tags only, not samples. Tag both the frames and encoder
    // so FFmpeg 8's format negotiation does not request a color conversion.
    let mut params = Vec::new();
    for (key, own, fallback) in [
        ("color_primaries", &info.primaries, &reference.primaries),
        ("color_trc", &info.transfer, &reference.transfer),
        ("colorspace", &info.matrix, &reference.matrix),
        ("range", &info.range, &reference.range),
    ] {
        let tag = if unknown(own) { fallback } else { own };
        if !unknown(tag) {
            params.push(format!("{key}={tag}"));
        }
    }
    if !params.is_empty() {
        filter.push_str(&format!(",setparams={}", params.join(":")));
    }
    captured(&mut command).args([
        "-hide_banner",
        "-nostdin",
        "-v",
        "error",
        "-y",
        "-xerror",
        "-threads",
        "1",
        "-noautorotate",
        "-r",
        &fps,
        "-i",
        input,
        "-map",
        "0:v:0",
        "-an",
        "-sn",
        "-dn",
        "-vf",
        &filter,
        "-fps_mode",
        "passthrough",
        "-c:v",
        "ffv1",
        "-level",
        "3",
        "-threads",
        "1",
        "-filter_threads",
        "1",
        "-pix_fmt",
        &format!("+{}", info.pixel_format),
    ]);
    // Elementary streams may omit color tags; inherit missing tags from the
    // reference, but retain any explicit trial tags for the metric to interpret.
    for (flag, own, fallback) in [
        ("-color_primaries", &info.primaries, &reference.primaries),
        ("-color_trc", &info.transfer, &reference.transfer),
        ("-colorspace", &info.matrix, &reference.matrix),
        ("-color_range", &info.range, &reference.range),
    ] {
        let tag = if unknown(own) { fallback } else { own };
        if !unknown(tag) {
            command.args([flag, tag]);
        }
    }
    command.arg("-f").arg("matroska").arg(output);
    command
}

fn cvvdp_command(
    config: &MetricConfig,
    reference: &str,
    distorted: &str,
    result: &Path,
    fps: f64,
) -> Command {
    let mut command = Command::new("cvvdp");
    captured(&mut command)
        .args([
            "--test",
            distorted,
            "--ref",
            reference,
            "--display",
            &config.display,
            "--device",
            &config.device,
            "--metric",
            "cvvdp",
            "--count-frames",
            // Elementary streams can have guessed rates. Reference timing governs
            // the temporal model; the validated pairs still align by frame index.
            "--fps",
            &fps.to_string(),
            "--result",
        ])
        .arg(result);
    command
}

async fn run(command: Command, label: &str) -> Result<Output, String> {
    let output = TrackedChild::spawn(command)
        .map_err(|e| format!("start {label}: {e}; check that the executable is installed on PATH"))?
        .wait_with_output()
        .await
        .map_err(|e| format!("wait for {label}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{label} failed ({}): {}",
            output.status,
            diagnostics(&output)
        ));
    }
    Ok(output)
}

fn diagnostics(output: &Output) -> String {
    format!(
        "stderr: {}; stdout: {}",
        tail(&output.stderr),
        tail(&output.stdout)
    )
}

fn tail(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(8192)..])
        .trim()
        .to_owned()
}

fn finite_score(score: f64, label: &str) -> Result<f64, String> {
    if score.is_finite() {
        Ok(score)
    } else {
        Err(format!("{label} returned a non-finite score: {score}"))
    }
}

fn parse_number(text: &str, label: &str) -> Result<f64, String> {
    let score = text
        .trim()
        .parse::<f64>()
        .map_err(|e| format!("invalid {label} score {text:?}: {e}"))?;
    finite_score(score, label)
}

fn parse_vmaf(stderr: &str) -> Result<f64, String> {
    let values: Vec<_> = stderr
        .lines()
        .filter_map(|line| line.split_once("VMAF score:").map(|(_, value)| value))
        .collect();
    if values.len() != 1 {
        return Err(format!(
            "expected one FFmpeg VMAF score, found {}; {}",
            values.len(),
            tail(stderr.as_bytes())
        ));
    }
    parse_number(values[0], "VMAF")
}

fn parse_cvvdp_csv(csv: &str) -> Result<f64, String> {
    let mut lines = csv.lines().filter(|line| !line.trim().is_empty());
    let header = lines.next().ok_or("CVVDP result CSV is empty")?;
    let columns: Vec<_> = header.split(',').map(str::trim).collect();
    if columns.len() != 3
        || columns[0] != "test"
        || columns[1] != "reference"
        || !columns[2].eq_ignore_ascii_case("cvvdp")
    {
        return Err(format!(
            "unexpected CVVDP CSV header: {header:?}; expected test, reference, CVVDP"
        ));
    }
    let row = lines.next().ok_or("CVVDP result CSV has no prediction")?;
    if lines.next().is_some() {
        return Err(
            "CVVDP result CSV contains multiple predictions; expected one video pair".into(),
        );
    }
    // Upstream emits unquoted paths, including commas; the metric is always last.
    let (_, value) = row
        .rsplit_once(',')
        .ok_or("CVVDP CSV prediction has no score column")?;
    parse_number(value, "CVVDP")
}

fn parse_butteraugli(stdout: &str) -> Result<f64, String> {
    // The first line is the distance, not the following `3-norm:` diagnostic.
    let first = stdout
        .lines()
        .next()
        .ok_or("butteraugli_main returned no distance")?;
    let score = parse_number(first, "Butteraugli")?;
    if score < 0.0 {
        return Err(format!("Butteraugli returned a negative distance: {score}"));
    }
    Ok(score)
}

#[derive(Debug)]
struct VideoInfo {
    width: u64,
    height: u64,
    frames: u64,
    fps: f64,
    pixel_format: String,
    primaries: String,
    transfer: String,
    matrix: String,
    range: String,
}

#[derive(Clone, Copy, Debug)]
enum Count {
    /// Decode every frame; used once per scene for the reference.
    Decoded,
    /// Demux only. IVF holds one temporal unit or superframe per shown frame,
    /// raw HEVC is parsed into access units, and FFV1 stores frames
    /// individually. The metric's own decoding (FFmpeg with -xerror, or
    /// Butteraugli's decoded frame count) catches undecodable trials.
    Packets,
}

impl Count {
    fn flag(self) -> &'static str {
        match self {
            Self::Decoded => "-count_frames",
            Self::Packets => "-count_packets",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Decoded => "nb_read_frames",
            Self::Packets => "nb_read_packets",
        }
    }
}

async fn probe(path: &str, count: Count) -> Result<VideoInfo, String> {
    let mut command = Command::new("ffprobe");
    let entries = format!(
        "stream=width,height,{},avg_frame_rate,r_frame_rate,pix_fmt,color_primaries,color_transfer,color_space,color_range",
        count.key()
    );
    captured(&mut command).args([
        "-v",
        "error",
        "-select_streams",
        "v:0",
        count.flag(),
        "-show_entries",
        &entries,
        "-of",
        "json",
        "-i",
        path,
    ]);
    let output = run(command, &format!("ffprobe for {path:?}")).await?;
    // ffprobe can report decoding errors but still exit successfully.
    if !output.stderr.is_empty() {
        return Err(format!(
            "ffprobe could not cleanly read {path:?}: {}",
            tail(&output.stderr)
        ));
    }
    parse_probe(&output.stdout, count).map_err(|e| format!("invalid video {path:?}: {e}"))
}

fn parse_probe(bytes: &[u8], count: Count) -> Result<VideoInfo, String> {
    let json: Value = serde_json::from_slice(bytes).map_err(|e| format!("ffprobe JSON: {e}"))?;
    let streams = json["streams"]
        .as_array()
        .ok_or("ffprobe has no streams array")?;
    if streams.len() != 1 {
        return Err("expected exactly one selected video stream".into());
    }
    let stream = &streams[0];
    let integer = |key: &str| -> Result<u64, String> {
        stream[key]
            .as_u64()
            .or_else(|| stream[key].as_str()?.parse().ok())
            .filter(|&n| n > 0)
            .ok_or_else(|| format!("missing/zero {key}; a complete decodable video is required"))
    };
    let text = |key: &str| stream[key].as_str().unwrap_or("unknown").to_owned();
    let fps = ["avg_frame_rate", "r_frame_rate"]
        .iter()
        .find_map(|key| {
            let (n, d) = stream[*key].as_str()?.split_once('/')?;
            let rate = n.parse::<f64>().ok()? / d.parse::<f64>().ok()?;
            (rate.is_finite() && rate > 0.0).then_some(rate)
        })
        .ok_or("missing valid video frame rate")?;
    Ok(VideoInfo {
        width: integer("width")?,
        height: integer("height")?,
        frames: integer(count.key())?,
        fps,
        pixel_format: text("pix_fmt"),
        primaries: text("color_primaries"),
        transfer: text("color_transfer"),
        matrix: text("color_space"),
        range: text("color_range"),
    })
}

fn validate_pair(reference: &VideoInfo, distorted: &VideoInfo) -> Result<(), String> {
    if (reference.width, reference.height) != (distorted.width, distorted.height) {
        return Err(format!("metric dimension mismatch: reference {}x{}, distorted {}x{}; inputs must not be resized", reference.width, reference.height, distorted.width, distorted.height));
    }
    if reference.frames != distorted.frames {
        return Err(format!("metric frame count mismatch: reference {}, distorted {}; refusing to score a truncated pair", reference.frames, distorted.frames));
    }
    Ok(())
}

#[derive(Debug)]
struct SdrColor {
    transfer: String,
    matrix: String,
    range: String,
}

fn unknown(tag: &str) -> bool {
    matches!(tag, "unknown" | "unspecified" | "reserved" | "")
}

impl SdrColor {
    fn from_info(info: &VideoInfo, fallback: Option<&Self>) -> Result<Self, String> {
        let rgb = info.pixel_format.starts_with("rgb")
            || info.pixel_format.starts_with("bgr")
            || info.pixel_format.starts_with("gbr");
        let transfer = if unknown(&info.transfer) {
            fallback.map(|c| c.transfer.as_str()).unwrap_or(if rgb {
                "iec61966-2-1"
            } else {
                "bt709"
            })
        } else {
            &info.transfer
        };
        let matrix = if rgb {
            "gbr"
        } else if unknown(&info.matrix) {
            fallback
                .filter(|c| c.matrix != "gbr")
                .map(|c| c.matrix.as_str())
                .unwrap_or("bt709")
        } else {
            &info.matrix
        };
        let range = if unknown(&info.range) {
            if rgb {
                "pc"
            } else {
                fallback.map(|c| c.range.as_str()).unwrap_or("tv")
            }
        } else {
            &info.range
        };
        if (!unknown(&info.primaries) && info.primaries != "bt709")
            || !matches!(transfer, "bt709" | "iec61966-2-1")
            || !matches!(matrix, "bt709" | "gbr")
            || !matches!(range, "tv" | "pc")
        {
            return Err(format!("Butteraugli supports only BT.709/sRGB SDR; HDR/wide-gamut or unsupported color tags found: primaries={}, transfer={}, matrix={}, range={}. Use a metric/display with HDR support, or explicitly convert to SDR before encoding; no implicit tone mapping is performed", info.primaries, info.transfer, info.matrix, info.range));
        }
        Ok(Self {
            transfer: transfer.into(),
            matrix: matrix.into(),
            range: range.into(),
        })
    }

    fn filter(&self) -> String {
        format!("zscale=pin=bt709:tin={}:min={}:rin={}:p=bt709:t=iec61966-2-1:m=gbr:r=full,format=gbrp16le,format=rgb48be", self.transfer, self.matrix, if self.range == "pc" { "full" } else { "limited" })
    }
}

fn png_command(path: &str, color: &SdrColor) -> Command {
    let mut command = Command::new("ffmpeg");
    captured(&mut command).args([
        "-hide_banner",
        "-nostdin",
        "-v",
        "error",
        "-xerror",
        "-threads",
        "1",
        "-noautorotate",
        "-i",
        path,
        "-map",
        "0:v:0",
        "-an",
        "-sn",
        "-dn",
        "-vf",
        &color.filter(),
        "-fps_mode",
        "passthrough",
        "-threads",
        "1",
        "-filter_threads",
        "1",
        "-c:v",
        "png",
        // Deflate dominates 16-bit PNG encoding (about 50x slower at the
        // default level) and the files are only read back once.
        "-compression_level",
        "0",
        "-pix_fmt",
        "rgb48be",
        "-f",
        "image2pipe",
        "pipe:1",
    ]);
    command
}

struct PngStream {
    child: TrackedChild,
    stdout: ChildStdout,
    stderr: JoinHandle<std::io::Result<Vec<u8>>>,
}

impl PngStream {
    fn start(path: &str, color: &SdrColor) -> Result<Self, String> {
        let mut child = TrackedChild::spawn(png_command(path, color))
            .map_err(|e| format!("start FFmpeg RGB16 PNG decoder for {path:?}: {e}"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or("FFmpeg PNG decoder has no stdout")?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or("FFmpeg PNG decoder has no stderr")?;
        let stderr = tokio::spawn(async move {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes).await?;
            Ok(bytes)
        });
        Ok(Self {
            child,
            stdout,
            stderr,
        })
    }

    async fn finish(mut self) -> Result<(), String> {
        let status = self
            .child
            .wait()
            .await
            .map_err(|e| format!("wait for FFmpeg PNG decoder: {e}"))?;
        let stderr = (&mut self.stderr)
            .await
            .map_err(|e| format!("read FFmpeg decoder diagnostics: {e}"))?
            .map_err(|e| format!("read FFmpeg decoder stderr: {e}"))?;
        if !status.success() {
            return Err(format!(
                "FFmpeg RGB16 PNG decoder failed ({status}); requires zscale and PNG support: {}",
                tail(&stderr)
            ));
        }
        Ok(())
    }
}

/// Read exactly one PNG from image2pipe. Chunks are bounded before allocating;
/// IHDR is checked to catch resolution changes and accidental 8-bit conversion.
async fn read_png<R: AsyncRead + Unpin>(
    reader: &mut R,
    info: &VideoInfo,
) -> Result<Option<Vec<u8>>, String> {
    let mut signature = [0u8; 8];
    let read = reader
        .read(&mut signature[..1])
        .await
        .map_err(|e| format!("read PNG stream: {e}"))?;
    if read == 0 {
        return Ok(None);
    }
    reader
        .read_exact(&mut signature[1..])
        .await
        .map_err(|e| format!("truncated PNG signature: {e}"))?;
    if signature != *b"\x89PNG\r\n\x1a\n" {
        return Err("invalid PNG stream signature".into());
    }
    // Generous bound for uncompressed RGB16, filter bytes, and PNG metadata.
    let limit = info
        .width
        .checked_mul(info.height)
        .and_then(|n| n.checked_mul(8))
        .and_then(|n| n.checked_add(1_048_576))
        .and_then(|n| usize::try_from(n).ok())
        .ok_or("video dimensions overflow PNG buffer size")?;
    let mut png = signature.to_vec();
    let mut first = true;
    loop {
        let mut header = [0u8; 8];
        reader
            .read_exact(&mut header)
            .await
            .map_err(|e| format!("truncated PNG chunk: {e}"))?;
        let length = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
        let new_len = png
            .len()
            .checked_add(12)
            .and_then(|n| n.checked_add(length))
            .filter(|&n| n <= limit)
            .ok_or("PNG frame exceeds bounded RGB16 buffer size")?;
        let start = png.len();
        png.extend_from_slice(&header);
        png.resize(new_len, 0);
        reader
            .read_exact(&mut png[start + 8..])
            .await
            .map_err(|e| format!("truncated PNG chunk payload: {e}"))?;
        if first {
            if &header[4..] != b"IHDR" || length != 13 {
                return Err("PNG stream missing IHDR".into());
            }
            let data = &png[start + 8..start + 21];
            let width = u32::from_be_bytes(data[..4].try_into().unwrap()) as u64;
            let height = u32::from_be_bytes(data[4..8].try_into().unwrap()) as u64;
            if (width, height) != (info.width, info.height) || data[8] != 16 || data[9] != 2 {
                return Err(format!(
                    "expected {}x{} 16-bit RGB PNG, got {width}x{height} depth={} color_type={}",
                    info.width, info.height, data[8], data[9]
                ));
            }
            first = false;
        }
        if &header[4..] == b"IEND" {
            if length != 0 {
                return Err("invalid PNG IEND length".into());
            }
            return Ok(Some(png));
        }
    }
}

#[derive(Default)]
struct MeanDistance {
    count: u64,
    mean: f64,
}

impl MeanDistance {
    fn push(&mut self, distance: f64) -> Result<(), String> {
        finite_score(distance, "Butteraugli frame")?;
        if distance < 0.0 {
            return Err("Butteraugli frame distance must be nonnegative".into());
        }
        self.count += 1;
        // Incremental arithmetic mean avoids a sum overflowing on long videos.
        self.mean += (distance - self.mean) / self.count as f64;
        Ok(())
    }

    fn finish(&self, expected: u64) -> Result<f64, String> {
        if self.count == 0 || self.count != expected {
            return Err(format!(
                "Butteraugli evaluated {} frames, expected {expected}",
                self.count
            ));
        }
        finite_score(self.mean, "Butteraugli mean")
    }
}

/// Read the next frame from both decoders, which must end together.
async fn read_pair<R: AsyncRead + Unpin>(
    reference: &mut R,
    distorted: &mut R,
    info: &VideoInfo,
) -> Result<Option<(Vec<u8>, Vec<u8>)>, String> {
    match tokio::try_join!(read_png(reference, info), read_png(distorted, info))? {
        (None, None) => Ok(None),
        (Some(r), Some(d)) => Ok(Some((r, d))),
        _ => Err("Butteraugli reference and trial decoders produced different frame counts; refusing partial score".into()),
    }
}

async fn butteraugli(
    reference: &str,
    distorted: &str,
    work_dir: &Path,
    info: &VideoInfo,
    ref_color: &SdrColor,
    test_color: &SdrColor,
) -> Result<f64, String> {
    let mut reference = PngStream::start(reference, ref_color)?;
    let mut distorted = PngStream::start(distorted, test_color)?;
    let ref_png = work_dir.join("butteraugli-reference.png");
    let test_png = work_dir.join("butteraugli-distorted.png");
    let mut mean = MeanDistance::default();
    let scoring = async {
        let mut next = read_pair(&mut reference.stdout, &mut distorted.stdout, info).await?;
        let mut decoded = 0u64;
        while let Some((r, d)) = next {
            decoded += 1;
            if decoded > info.frames {
                return Err("Butteraugli decoded more frames than the preflight count; refusing partial score".into());
            }
            tokio::try_join!(tokio::fs::write(&ref_png, r), tokio::fs::write(&test_png, d))
                .map_err(|e| format!("write Butteraugli frame pair: {e}"))?;
            let mut command = Command::new("butteraugli_main");
            captured(&mut command)
                .arg(&ref_png)
                .arg(&test_png)
                .args(["--colorspace", "RGB_D65_SRG_Rel_SRG"]);
            // Decode the following pair while this one is scored.
            let (output, following) = tokio::join!(
                run(command, "libjxl butteraugli_main (requires PNG support)"),
                read_pair(&mut reference.stdout, &mut distorted.stdout, info),
            );
            let output = output?;
            let distance = parse_butteraugli(&String::from_utf8_lossy(&output.stdout))
                .map_err(|e| format!("{e}; {}", diagnostics(&output)))?;
            mean.push(distance)?;
            next = following?;
        }
        Ok::<(), String>(())
    }
    .await;
    if let Err(error) = scoring {
        // Stop readers before waiting: either decoder may be blocked on stdout.
        let _ = reference.child.start_kill();
        let _ = distorted.child.start_kill();
        let (r, d) = tokio::join!(reference.finish(), distorted.finish());
        return Err(format!(
            "{error}; reference decoder: {}; distorted decoder: {}",
            r.err().unwrap_or_default(),
            d.err().unwrap_or_default()
        ));
    }
    let (r, d) = tokio::join!(reference.finish(), distorted.finish());
    r?;
    d?;
    mean.finish(info.frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(kind: MetricKind) -> MetricConfig {
        MetricConfig {
            kind,
            target: 1.5,
            tolerance: 0.1,
            display: "standard_fhd".into(),
            device: "cpu".into(),
        }
    }

    fn args(command: &Command) -> Vec<String> {
        command
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn info() -> VideoInfo {
        parse_probe(br#"{"streams":[{"width":64,"height":48,"nb_read_frames":"3","avg_frame_rate":"24/1","pix_fmt":"yuv420p","color_primaries":"bt709","color_transfer":"bt709","color_space":"bt709","color_range":"tv"}]}"#, Count::Decoded).unwrap()
    }

    #[test]
    fn trials_count_packets_without_decoding() {
        let packets = br#"{"streams":[{"width":64,"height":48,"nb_read_packets":"3","avg_frame_rate":"24/1"}]}"#;
        assert_eq!(parse_probe(packets, Count::Packets).unwrap().frames, 3);
        assert!(parse_probe(packets, Count::Decoded).is_err());
        assert_eq!(Count::Packets.flag(), "-count_packets");
        assert_eq!(Count::Decoded.flag(), "-count_frames");
    }

    #[test]
    fn native_units_and_search_direction() {
        for kind in [MetricKind::Vmaf, MetricKind::Cvvdp, MetricKind::Butteraugli] {
            let c = config(kind);
            assert_eq!(
                c.search_target(),
                if kind == MetricKind::Butteraugli {
                    -1.5
                } else {
                    1.5
                }
            );
            assert_eq!(c.search_tolerance(), 0.1);
            assert!(!kind.name().is_empty());
        }
        assert_eq!(
            parse_vmaf("[libvmaf] VMAF score: 95.125000\n").unwrap(),
            95.125
        );
    }

    #[test]
    fn pairs_vmaf_frames_by_index() {
        let command = vmaf_command("ref with spaces.mkv", "trial.ivf");
        assert_eq!(command.as_std().get_program(), "ffmpeg");
        let args = args(&command);
        assert!(args.windows(2).any(|a| a == ["-xerror", "-i"]));
        assert!(args.windows(2).any(|a| a == ["-i", "trial.ivf"]));
        assert!(args
            .windows(4)
            .any(|a| a == ["-threads", "4", "-i", "ref with spaces.mkv"]));
        assert!(!args.iter().any(|a| a == "-filter_complex_threads"));
        let graph = &args[args.iter().position(|a| a == "-lavfi").unwrap() + 1];
        assert_eq!(graph, "[0:v:0]settb=AVTB,setpts=N[distorted];[1:v:0]settb=AVTB,setpts=N[reference];[distorted][reference]libvmaf=model='path=/usr/local/share/model/vmaf_v0.6.1.json'");
    }

    #[test]
    fn cvvdp_uses_official_device_and_structured_result() {
        let command = cvvdp_command(
            &config(MetricKind::Cvvdp),
            "ref.mkv",
            "test.hevc",
            Path::new("result.csv"),
            24.0,
        );
        let a = args(&command);
        for pair in [
            ["--device", "cpu"],
            ["--display", "standard_fhd"],
            ["--result", "result.csv"],
            ["--test", "test.hevc"],
            ["--ref", "ref.mkv"],
            ["--fps", "24"],
        ] {
            assert!(a.windows(2).any(|a| a == pair));
        }
        assert!(!a.iter().any(|a| a == "--gpu"));
        assert_eq!(
            parse_cvvdp_csv("test, reference, CVVDP\ncomma,in,test, ref, 9.1256\n").unwrap(),
            9.1256
        );
    }

    #[test]
    fn parsers_reject_missing_ambiguous_and_nonfinite_scores() {
        for text in [
            "",
            "VMAF score: NaN",
            "VMAF score: inf",
            "VMAF score: 1\nVMAF score: 2",
            "VMAF score: nonsense",
        ] {
            assert!(parse_vmaf(text).is_err(), "{text}");
        }
        for text in [
            "",
            "test, reference, PSNR\na,b,4",
            "test, reference, CVVDP\na,b,NaN",
            "test, reference, CVVDP\na,b,9\nc,d,8",
            "test, reference, CVVDP\n",
        ] {
            assert!(parse_cvvdp_csv(text).is_err(), "{text}");
        }
        assert_eq!(parse_butteraugli("1.25\n3-norm: 0.8\n").unwrap(), 1.25);
        for text in ["", "NaN", "inf", "-1", "3-norm: 1.2"] {
            assert!(parse_butteraugli(text).is_err());
        }
    }

    #[test]
    fn mean_includes_every_frame_and_validates_count() {
        let mut mean = MeanDistance::default();
        assert!(mean.finish(0).is_err());
        for value in [1.0, 2.0, 6.0] {
            mean.push(value).unwrap();
        }
        assert_eq!(mean.finish(3).unwrap(), 3.0);
        assert!(mean.finish(2).is_err());
        assert!(mean.push(f64::INFINITY).is_err());
        assert!(mean.push(-1.0).is_err());
    }

    #[test]
    fn validates_dimensions_counts_and_probe_errors() {
        let mut test = info();
        assert!(validate_pair(&info(), &test).is_ok());
        test.frames = 2;
        assert!(validate_pair(&info(), &test)
            .unwrap_err()
            .contains("frame count"));
        test.width = 32;
        assert!(validate_pair(&info(), &test)
            .unwrap_err()
            .contains("dimension"));
        for text in [
            b"{}".as_slice(),
            br#"{"streams":[]}"#,
            br#"{"streams":[{"nb_read_frames":"N/A"}]}"#,
        ] {
            assert!(parse_probe(text, Count::Decoded).is_err());
        }
    }

    #[test]
    fn sdr_conversion_is_explicit_and_hdr_is_rejected() {
        let mut i = info();
        let c = SdrColor::from_info(&i, None).unwrap();
        let a = args(&png_command("ref.mkv", &c));
        assert!(a
            .iter()
            .any(|a| a.contains("tin=bt709:min=bt709:rin=limited")));
        assert!(a.iter().any(|a| a.contains("t=iec61966-2-1:m=gbr:r=full")));
        assert!(a.windows(2).any(|a| a == ["-pix_fmt", "rgb48be"]));
        assert!(a.windows(2).any(|a| a == ["-compression_level", "0"]));
        for transfer in ["smpte2084", "arib-std-b67", "linear"] {
            i.transfer = transfer.into();
            assert!(SdrColor::from_info(&i, None).unwrap_err().contains("HDR"));
        }
        i.transfer = "unknown".into();
        assert_eq!(SdrColor::from_info(&i, Some(&c)).unwrap().transfer, "bt709");
        i.primaries = "bt2020".into();
        assert!(SdrColor::from_info(&i, None).is_err());
    }

    #[test]
    fn cvvdp_device_indices_translate_and_retiming_never_resamples() {
        assert_eq!(cvvdp_device("cpu").unwrap(), "cpu");
        assert_eq!(cvvdp_device("0").unwrap(), "cuda:0");
        assert_eq!(cvvdp_device("12").unwrap(), "cuda:12");
        for device in ["", "-1", "cuda:0", "garbage", "9999999999999999"] {
            assert!(cvvdp_device(device).is_err());
        }
        let a = args(&comparison_command(
            "trial.hevc",
            Path::new("comparison.mkv"),
            &info(),
            &info(),
        ));
        assert!(a.windows(2).any(|a| a == ["-fps_mode", "passthrough"]));
        assert!(a.windows(2).any(|a| a == ["-c:v", "ffv1"]));
        assert!(a.windows(2).any(|a| a == ["-pix_fmt", "+yuv420p"]));
        assert!(a
            .windows(2)
            .any(|a| a[0] == "-vf" && a[1].starts_with("settb=AVTB,setpts=N/(24*TB)")));
        assert!(!a.iter().any(|a| a.contains("fps=")));
    }

    #[test]
    fn config_validation_and_input_paths() {
        for kind in [MetricKind::Vmaf, MetricKind::Cvvdp, MetricKind::Butteraugli] {
            let c = config(kind);
            assert!(c.validate().is_ok());
            for (target, tolerance) in [
                (f64::NAN, 0.1),
                (f64::INFINITY, 0.1),
                (1.0, f64::NEG_INFINITY),
                (1.0, f64::NAN),
                (1.0, -0.1),
            ] {
                let mut invalid = c.clone();
                invalid.target = target;
                invalid.tolerance = tolerance;
                assert!(invalid
                    .validate()
                    .unwrap_err()
                    .contains("finite and nonnegative"));
            }
            let mut zero = c.clone();
            zero.tolerance = 0.0;
            assert!(zero.validate().is_ok());
        }
        let mut c = config(MetricKind::Cvvdp);
        c.display.clear();
        assert!(c.validate().unwrap_err().contains("display and device"));
        c.display = "standard_fhd".into();
        c.device.clear();
        assert!(c.validate().unwrap_err().contains("display and device"));
        c.device = "-1".into();
        assert!(c.validate().unwrap_err().contains("invalid CVVDP device"));
        c.device = "00012".into();
        assert!(c.validate().is_ok());
        assert_eq!(cvvdp_device(&c.device).unwrap(), "cuda:12");
        assert!(cvvdp_device("4294967296")
            .unwrap_err()
            .contains("too large"));

        assert_eq!(
            utf8_path(Path::new("work dir/test.mkv")).unwrap(),
            "work dir/test.mkv"
        );
        assert!(cvvdp_path("comma,in path.mkv").is_ok());
        for path in ["one\ntwo", "one\rtwo"] {
            assert!(cvvdp_path(path).unwrap_err().contains("newlines"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_work_path_is_rejected() {
        use std::os::unix::ffi::OsStrExt;
        let path = Path::new(std::ffi::OsStr::from_bytes(b"work/\xff"));
        assert!(utf8_path(path).unwrap_err().contains("must be UTF-8"));
    }

    #[tokio::test]
    async fn invalid_config_fails_before_reference_probe() {
        let mut c = config(MetricKind::Vmaf);
        c.target = f64::NAN;
        let error = c
            .prepare_reference("does-not-exist", Path::new("unused"))
            .await
            .unwrap_err();
        assert!(error.contains("metric target must be finite"));
        let prepared = PreparedReference {
            path: "unused".into(),
            info: info(),
            cvvdp: None,
            color: None,
        };
        let error = c
            .evaluate(&prepared, "does-not-exist", Path::new("unused"))
            .await
            .unwrap_err();
        assert!(error.contains("metric target must be finite"));
    }

    #[test]
    fn probe_parses_fallback_rates_and_reports_invalid_fields() {
        let data = br#"{"streams":[{"width":"64","height":48,"nb_read_packets":3,"avg_frame_rate":"0/0","r_frame_rate":"24000/1001"}]}"#;
        let video = parse_probe(data, Count::Packets).unwrap();
        assert_eq!((video.width, video.height, video.frames), (64, 48, 3));
        assert_eq!(video.fps, 24000.0 / 1001.0);
        assert_eq!(video.pixel_format, "unknown");
        assert_eq!(video.primaries, "unknown");
        assert_eq!(video.transfer, "unknown");
        assert_eq!(video.matrix, "unknown");
        assert_eq!(video.range, "unknown");

        for (json, message) in [
            ("not json", "ffprobe JSON"),
            (r#"{"streams":{}}"#, "no streams array"),
            (r#"{"streams":[{},{}]}"#, "exactly one"),
            (
                r#"{"streams":[{"width":0,"height":48,"nb_read_frames":3,"avg_frame_rate":"24/1"}]}"#,
                "missing/zero width",
            ),
            (
                r#"{"streams":[{"width":64,"height":"0","nb_read_frames":3,"avg_frame_rate":"24/1"}]}"#,
                "missing/zero height",
            ),
            (
                r#"{"streams":[{"width":64,"height":48,"nb_read_frames":"N/A","avg_frame_rate":"24/1"}]}"#,
                "missing/zero nb_read_frames",
            ),
            (
                r#"{"streams":[{"width":64,"height":48,"nb_read_frames":3,"avg_frame_rate":"24/0","r_frame_rate":"-1/1"}]}"#,
                "missing valid video frame rate",
            ),
            (
                r#"{"streams":[{"width":64,"height":48,"nb_read_frames":3,"avg_frame_rate":"bad","r_frame_rate":"0/1"}]}"#,
                "missing valid video frame rate",
            ),
        ] {
            let error = parse_probe(json.as_bytes(), Count::Decoded).unwrap_err();
            assert!(error.contains(message), "{json}: {error}");
        }
    }

    #[test]
    fn sdr_defaults_inheritance_and_invalid_tags() {
        let mut video = info();
        video.primaries = "unspecified".into();
        video.transfer = "reserved".into();
        video.matrix = "unknown".into();
        video.range.clear();
        let yuv = SdrColor::from_info(&video, None).unwrap();
        assert_eq!(
            (&*yuv.transfer, &*yuv.matrix, &*yuv.range),
            ("bt709", "bt709", "tv")
        );
        for pixel_format in ["rgb24", "bgr24", "gbrp"] {
            video.pixel_format = pixel_format.into();
            let rgb = SdrColor::from_info(&video, None).unwrap();
            assert_eq!(
                (&*rgb.transfer, &*rgb.matrix, &*rgb.range),
                ("iec61966-2-1", "gbr", "pc")
            );
            assert!(rgb.filter().contains("tin=iec61966-2-1:min=gbr:rin=full"));
            let inherited = SdrColor::from_info(&video, Some(&yuv)).unwrap();
            assert_eq!(
                (&*inherited.transfer, &*inherited.matrix, &*inherited.range),
                ("bt709", "gbr", "pc")
            );
        }
        let rgb = SdrColor::from_info(&video, None).unwrap();
        video.pixel_format = "yuv420p".into();
        let inherited = SdrColor::from_info(&video, Some(&rgb)).unwrap();
        assert_eq!(
            (&*inherited.transfer, &*inherited.matrix, &*inherited.range),
            ("iec61966-2-1", "bt709", "pc")
        );
        video.matrix = "bt2020nc".into();
        assert!(SdrColor::from_info(&video, None)
            .unwrap_err()
            .contains("matrix=bt2020nc"));
        video.matrix = "bt709".into();
        video.range = "invalid".into();
        assert!(SdrColor::from_info(&video, None)
            .unwrap_err()
            .contains("range=invalid"));
    }

    #[test]
    fn comparison_command_inherits_only_missing_color_tags() {
        let mut trial = info();
        trial.primaries = "unknown".into();
        trial.transfer = "unspecified".into();
        trial.matrix = "bt2020nc".into();
        trial.range = "reserved".into();
        let mut reference = info();
        reference.transfer = "unknown".into();
        let a = args(&comparison_command(
            "input",
            Path::new("out.mkv"),
            &trial,
            &reference,
        ));
        assert!(a.windows(2).any(|p| p == ["-color_primaries", "bt709"]));
        assert!(!a.iter().any(|arg| arg == "-color_trc"));
        assert!(a.windows(2).any(|p| p == ["-colorspace", "bt2020nc"]));
        assert!(a.windows(2).any(|p| p == ["-color_range", "tv"]));
        let filter = &a[a.iter().position(|arg| arg == "-vf").unwrap() + 1];
        assert!(filter.contains("setparams=color_primaries=bt709:colorspace=bt2020nc:range=tv"));
        for tag in [
            &mut trial.primaries,
            &mut trial.transfer,
            &mut trial.matrix,
            &mut trial.range,
            &mut reference.primaries,
            &mut reference.transfer,
            &mut reference.matrix,
            &mut reference.range,
        ] {
            *tag = "unknown".into();
        }
        let a = args(&comparison_command(
            "input",
            Path::new("out.mkv"),
            &trial,
            &reference,
        ));
        let filter = &a[a.iter().position(|arg| arg == "-vf").unwrap() + 1];
        assert!(!filter.contains("setparams"));
        assert!(!a.iter().any(|arg| arg == "-color_range"));
    }

    #[test]
    fn csv_and_diagnostic_edge_cases() {
        assert_eq!(
            parse_cvvdp_csv("\n test, reference, CvVdP \n\n a,b, 0 \n\n").unwrap(),
            0.0
        );
        assert!(parse_cvvdp_csv("test,reference,cvvdp\nno-score")
            .unwrap_err()
            .contains("no score column"));
        assert_eq!(parse_butteraugli("0\n3-norm: 2").unwrap(), 0.0);
        let bytes = [vec![b'x'; 100], vec![b'y'; 8192]].concat();
        assert_eq!(tail(&bytes), "y".repeat(8192));
        assert_eq!(tail(b"  diagnostic \n"), "diagnostic");
        assert_eq!(tail(b"\xff"), "\u{fffd}");
    }

    #[tokio::test]
    async fn png_stream_reports_chunk_errors_and_dimension_overflow() {
        let png = png_fixture(16);
        let mut missing_ihdr = png.clone();
        missing_ihdr[12..16].copy_from_slice(b"IDAT");
        let mut wrong_color = png.clone();
        wrong_color[25] = 6;
        let mut bad_iend = png.clone();
        bad_iend[33..37].copy_from_slice(&1u32.to_be_bytes());
        bad_iend.push(0);
        for (bytes, message) in [
            (&png[..10], "truncated PNG chunk"),
            (&png[..20], "truncated PNG chunk payload"),
            (missing_ihdr.as_slice(), "missing IHDR"),
            (wrong_color.as_slice(), "color_type=6"),
            (bad_iend.as_slice(), "invalid PNG IEND length"),
        ] {
            let error = read_png(&mut &bytes[..], &info()).await.unwrap_err();
            assert!(error.contains(message), "{error}");
        }
        let mut huge = info();
        huge.width = u64::MAX;
        assert!(read_png(&mut png.as_slice(), &huge)
            .await
            .unwrap_err()
            .contains("overflow"));
        let mut reference = png.as_slice();
        let mut distorted = b"bad".as_slice();
        assert!(read_pair(&mut reference, &mut distorted, &info())
            .await
            .unwrap_err()
            .contains("truncated PNG signature"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn subprocess_success_captures_stdout_and_stderr() {
        let mut command = Command::new("sh");
        captured(&mut command).args(["-c", "printf 'score'; printf ' warning ' >&2"]);
        let output = run(command, "test metric").await.unwrap();
        assert_eq!(output.stdout, b"score");
        assert_eq!(diagnostics(&output), "stderr: warning; stdout: score");
    }

    // Each scenario scopes its mock environment to this task. No process-wide
    // environment is changed, so these tests can run alongside real subprocesses.
    #[cfg(unix)]
    #[tokio::test]
    async fn mock_backends_cover_preparation_and_evaluation() {
        use std::os::unix::fs::PermissionsExt;

        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        let root = std::env::temp_dir().join(format!("sav1n-metric-mocks-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let _cleanup = Cleanup(root.clone());
        let bin = root.join("bin");
        std::fs::create_dir(&bin).unwrap();
        let install = |name: &str, script: &str| {
            let path = bin.join(name);
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        install(
            "ffprobe",
            r##"#!/bin/sh
for arg do last=$arg; done
case "$MOCK_SCENARIO" in
  probe_failure) printf 'probe failed' >&2; exit 3 ;;
  probe_stderr) printf 'decode warning' >&2; exit 0 ;;
  probe_bad_json) printf '{'; exit 0 ;;
esac
width=64 height=48 frames=3 rate=24/1 primaries=bt709
case "$MOCK_SCENARIO" in
  smoke_vmaf|smoke_cvvdp|smoke_butteraugli)
    height=64; frames=8; rate=24000/1001
    case "$last" in *short.mkv) frames=7 ;; esac ;;
  smoke_retime) rate=24000/1001 ;;
esac
case "$MOCK_SCENARIO:$last" in
  dimension_mismatch:*trial.mkv) width=32 ;;
  frame_mismatch:*trial.mkv) frames=2 ;;
  retime_ref_bad_frames:*cvvdp-reference.mkv) frames=2 ;;
  retime_trial_bad_frames:*cvvdp-distorted.mkv) frames=2 ;;
  retimed_fps_mismatch:*cvvdp-distorted.mkv) rate=25/1 ;;
  prepare_color_error:*) primaries=bt2020 ;;
  butter_color_invalid:*trial.mkv) primaries=bt2020 ;;
esac
printf '{"streams":[{"width":%s,"height":%s,"nb_read_frames":"%s","nb_read_packets":"%s","avg_frame_rate":"%s","pix_fmt":"yuv420p","color_primaries":"%s","color_transfer":"bt709","color_space":"bt709","color_range":"tv"}]}' "$width" "$height" "$frames" "$frames" "$rate" "$primaries"
"##,
        );
        let encode_png = |png: &[u8]| png.iter().map(|b| format!("\\{b:03o}")).collect::<String>();
        let png = png_fixture(16);
        let mut square_png = png.clone();
        square_png[20..24].copy_from_slice(&64u32.to_be_bytes());
        let ffmpeg = r##"#!/bin/sh
png=0 md5=0 prev= input= first_input= last=
for arg do
  if [ "$prev" = -i ]; then
    input=$arg
    if [ -z "$first_input" ]; then first_input=$arg; fi
  fi
  if [ "$arg" = image2pipe ]; then png=1; fi
  if [ "$arg" = framemd5 ]; then md5=1; fi
  prev=$arg; last=$arg
done
if [ "$png" = 1 ]; then
  count=3 square=0
  case "$MOCK_SCENARIO" in
    butter_too_many) count=4 ;;
    butter_too_few) count=2 ;;
    butter_stream_mismatch) case "$input" in *trial.mkv) count=2 ;; esac ;;
    butter_stream_invalid) printf 'bad'; exit 0 ;;
    smoke_butteraugli) count=8; square=1 ;;
  esac
  while [ "$count" -gt 0 ]; do
    if [ "$square" = 1 ]; then printf 'SQUARE_PNG_BYTES'; else printf 'PNG_BYTES'; fi
    count=$((count - 1))
  done
  if [ "$MOCK_SCENARIO" = butter_decoder_failure ]; then
    printf 'decoder broke' >&2; exit 4
  fi
  exit 0
fi
case "$MOCK_SCENARIO:$last" in
  retime_ref_fail:*cvvdp-reference.mkv|retime_trial_fail:*cvvdp-distorted.mkv)
    printf 'retime failed' >&2; exit 5 ;;
  vmaf_failure:-) printf 'vmaf failed' >&2; exit 6 ;;
  vmaf_invalid:-) printf 'VMAF score: NaN\n' >&2; exit 0 ;;
esac
if [ "$md5" = 1 ]; then
  printf '# frame hashes\n0,0,0,1,4608,hash1\n0,1,1,1,4608,hash2\n0,2,2,1,4608,hash3\n'
elif [ "$last" = - ]; then
  score=94.5
  case "$first_input" in *degraded.mkv) score=50 ;; esac
  printf '[libvmaf] VMAF score: %s\n' "$score" >&2
else
  : > "$last"
fi
"##
        .replace("SQUARE_PNG_BYTES", &encode_png(&square_png))
        .replace("PNG_BYTES", &encode_png(&png));
        install("ffmpeg", &ffmpeg);
        install(
            "cvvdp",
            r##"#!/bin/sh
while [ "$#" -gt 0 ]; do
  case "$1" in
    --result) result=$2; shift 2 ;;
    --device) device=$2; shift 2 ;;
    --fps) fps=$2; shift 2 ;;
    --display) display=$2; shift 2 ;;
    *) shift ;;
  esac
done
case "$MOCK_SCENARIO:$fps:$display" in
  smoke_cvvdp:23.976*:standard_fhd|*:24:standard_fhd) ;;
  *) printf 'wrong CVVDP arguments' >&2; exit 8 ;;
esac
case "$MOCK_SCENARIO" in
  cvvdp_failure) printf 'cvvdp failed' >&2; exit 7 ;;
  cvvdp_gpu_fallback)
    if [ "$device" != cuda:2 ]; then printf 'wrong GPU device' >&2; exit 8; fi
    printf 'reverting to CPU' >&2; exit 0 ;;
  cvvdp_missing_result) printf 'no result' >&2; exit 0 ;;
  cvvdp_bad_result) printf 'test,reference,cvvdp\na,b,NaN\n' > "$result"; exit 0 ;;
esac
if [ "$device" != cpu ]; then printf 'wrong CPU device' >&2; exit 8; fi
score=8.75
case "$result" in */degraded/*) score=5 ;; esac
printf 'test,reference,cvvdp\na,b,%s\n' "$score" > "$result"
"##,
        );
        install(
            "butteraugli_main",
            r##"#!/bin/sh
if [ "$3" != --colorspace ] || [ "$4" != RGB_D65_SRG_Rel_SRG ]; then
  printf 'wrong colorspace' >&2; exit 8
fi
case "$MOCK_SCENARIO" in
  butter_failed) printf 'butter failed' >&2; exit 9 ;;
  butter_score_error) printf 'NaN\n'; exit 0 ;;
  butter_success)
    counter="${0%/*}/../butter-frame-count"
    count=0
    if [ -f "$counter" ]; then read -r count < "$counter"; fi
    count=$((count + 1))
    printf '%s\n' "$count" > "$counter"
    printf '%s\n3-norm: 0.5\n' "$count"
    exit 0 ;;
esac
score=1.25
case "$1" in */degraded/*) score=3 ;; esac
printf '%s\n3-norm: 0.5\n' "$score"
"##,
        );
        let scenarios = [
            "probe_failure",
            "probe_stderr",
            "probe_bad_json",
            "reference_newline",
            "prepare_dir_error",
            "prepare_color_error",
            "retime_ref_fail",
            "retime_ref_bad_frames",
            "prepare_non_utf8",
            "vmaf_success",
            "vmaf_failure",
            "vmaf_invalid",
            "dimension_mismatch",
            "frame_mismatch",
            "evaluate_workdir_error",
            "unprepared_cvvdp",
            "unprepared_butter",
            "trial_newline",
            "stale_result_remove_error",
            "retime_trial_fail",
            "retime_trial_bad_frames",
            "retimed_fps_mismatch",
            "cvvdp_trial_non_utf8",
            "cvvdp_success",
            "cvvdp_failure",
            "cvvdp_gpu_fallback",
            "cvvdp_missing_result",
            "cvvdp_bad_result",
            "butter_success",
            "butter_color_invalid",
            "butter_failed",
            "butter_score_error",
            "butter_too_many",
            "butter_too_few",
            "butter_stream_mismatch",
            "butter_stream_invalid",
            "butter_decoder_failure",
            "butter_write_error",
        ];
        for scenario in scenarios {
            let work = root.join(scenario);
            std::fs::create_dir(&work).unwrap();
            TEST_METRIC_ENV
                .scope((bin.clone(), scenario.into()), async {
                    mock_metric_scenario(scenario, &work).await;
                })
                .await;
        }
        // Exercise the smoke-test orchestration with fakes; these runs do not
        // validate metric quality or FFmpeg's frame preservation. The ignored
        // real-tool tests below perform those checks separately.
        for kind in [MetricKind::Vmaf, MetricKind::Cvvdp, MetricKind::Butteraugli] {
            TEST_METRIC_ENV
                .scope((bin.clone(), format!("smoke_{}", kind.name())), async {
                    real_metric_smoke(kind).await;
                })
                .await;
        }
        TEST_METRIC_ENV
            .scope((bin, "smoke_retime".into()), async {
                lossless_retime_smoke().await;
            })
            .await;
    }

    #[cfg(unix)]
    async fn mock_metric_scenario(scenario: &str, work: &Path) {
        use std::os::unix::ffi::OsStringExt;
        let non_utf8 = || work.join(std::ffi::OsString::from_vec(b"non-utf8-\xff".to_vec()));
        let mut c = config(
            if scenario.starts_with("butter_") || scenario == "prepare_color_error" {
                MetricKind::Butteraugli
            } else if scenario.starts_with("cvvdp_")
                || scenario.starts_with("retime")
                || matches!(
                    scenario,
                    "reference_newline"
                        | "prepare_dir_error"
                        | "prepare_non_utf8"
                        | "trial_newline"
                        | "stale_result_remove_error"
                )
            {
                MetricKind::Cvvdp
            } else {
                MetricKind::Vmaf
            },
        );
        if scenario == "cvvdp_gpu_fallback" {
            c.device = "2".into();
        }
        let ref_work = if scenario == "prepare_non_utf8" {
            non_utf8()
        } else {
            work.join("reference")
        };
        if scenario == "prepare_dir_error" {
            std::fs::write(&ref_work, b"file").unwrap();
        }
        let reference = if scenario == "reference_newline" {
            "ref\n.mkv"
        } else {
            "ref.mkv"
        };
        let prepared = c.prepare_reference(reference, &ref_work).await;
        let expected_prepare_error = match scenario {
            "probe_failure" => Some("probe failed"),
            "probe_stderr" => Some("could not cleanly read"),
            "probe_bad_json" => Some("ffprobe JSON"),
            "reference_newline" => Some("cannot contain newlines"),
            "prepare_dir_error" => Some("create metric work directory"),
            "prepare_color_error" => Some("HDR/wide-gamut"),
            "retime_ref_fail" => Some("retime failed"),
            "retime_ref_bad_frames" => Some("frame count mismatch"),
            "prepare_non_utf8" => Some("must be UTF-8"),
            _ => None,
        };
        if let Some(message) = expected_prepare_error {
            let error = prepared.unwrap_err();
            assert!(error.contains(message), "{scenario}: {error}");
            return;
        }
        let prepared = prepared.unwrap();
        if scenario == "unprepared_cvvdp" {
            c.kind = MetricKind::Cvvdp;
        }
        if scenario == "unprepared_butter" {
            c.kind = MetricKind::Butteraugli;
        }
        let eval_work = if scenario == "cvvdp_trial_non_utf8" {
            non_utf8()
        } else {
            work.join("evaluation")
        };
        if scenario == "evaluate_workdir_error" {
            std::fs::write(&eval_work, b"file").unwrap();
        }
        if matches!(
            scenario,
            "stale_result_remove_error"
                | "cvvdp_success"
                | "cvvdp_missing_result"
                | "butter_write_error"
        ) {
            std::fs::create_dir_all(&eval_work).unwrap();
            if scenario == "stale_result_remove_error" {
                std::fs::create_dir(eval_work.join("cvvdp-result.csv")).unwrap();
            } else if scenario == "butter_write_error" {
                std::fs::create_dir(eval_work.join("butteraugli-reference.png")).unwrap();
            } else {
                std::fs::write(eval_work.join("cvvdp-result.csv"), b"stale result").unwrap();
            }
        }
        let trial = if scenario == "trial_newline" {
            "trial\n.mkv"
        } else {
            "trial.mkv"
        };
        let result = c.evaluate(&prepared, trial, &eval_work).await;
        let expected_error = match scenario {
            "vmaf_failure" => Some("vmaf failed"),
            "vmaf_invalid" => Some("non-finite score"),
            "dimension_mismatch" => Some("dimension mismatch"),
            "frame_mismatch" => Some("frame count mismatch"),
            "evaluate_workdir_error" => Some("create metric work directory"),
            "unprepared_cvvdp" => Some("reference was not prepared for CVVDP"),
            "unprepared_butter" => Some("reference was not prepared for Butteraugli"),
            "trial_newline" => Some("cannot contain newlines"),
            "stale_result_remove_error" => Some("remove stale CVVDP result"),
            "retime_trial_fail" => Some("retime failed"),
            "retime_trial_bad_frames" => Some("frame count mismatch"),
            "retimed_fps_mismatch" => Some("different frame rates after retiming"),
            "cvvdp_trial_non_utf8" => Some("must be UTF-8"),
            "cvvdp_failure" => Some("cvvdp failed"),
            "cvvdp_gpu_fallback" => Some("could not use requested GPU 2"),
            "cvvdp_missing_result" => Some("did not produce readable"),
            "cvvdp_bad_result" => Some("non-finite score"),
            "butter_color_invalid" => Some("HDR/wide-gamut"),
            "butter_failed" => Some("butter failed"),
            "butter_score_error" => Some("non-finite score"),
            "butter_too_many" => Some("decoded more frames"),
            "butter_too_few" => Some("evaluated 2 frames, expected 3"),
            "butter_stream_mismatch" => Some("different frame counts"),
            "butter_stream_invalid" => Some("truncated PNG signature"),
            "butter_decoder_failure" => Some("decoder broke"),
            "butter_write_error" => Some("write Butteraugli frame pair"),
            _ => None,
        };
        if let Some(message) = expected_error {
            let error = result.unwrap_err();
            assert!(error.contains(message), "{scenario}: {error}");
        } else {
            let expected = match scenario {
                "vmaf_success" => 94.5,
                "cvvdp_success" => 8.75,
                // The mock returns frame distances 1, 2 and 3.
                "butter_success" => 2.0,
                _ => panic!("unknown scenario: {scenario}"),
            };
            assert_eq!(result.unwrap(), expected);
        }
    }

    fn smoke_dir(name: &str) -> std::path::PathBuf {
        let variant = TEST_METRIC_ENV
            .try_with(|(_, scenario)| scenario.clone())
            .unwrap_or_else(|_| "real".into());
        std::env::temp_dir().join(format!("sav1n-{name}-{}-{variant}", std::process::id()))
    }

    #[tokio::test]
    #[ignore = "requires real FFmpeg and ffprobe with FFV1 and lavfi"]
    async fn real_lossless_retime_smoke() {
        lossless_retime_smoke().await;
    }

    async fn lossless_retime_smoke() {
        let dir = smoke_dir("retime-smoke");
        tokio::fs::create_dir(&dir).await.unwrap();
        let source = dir.join("source.mkv");
        let output = dir.join("retimed.mkv");
        let mut command = Command::new("ffmpeg");
        captured(&mut command)
            .args([
                "-v",
                "error",
                "-nostdin",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=64x48:rate=25",
                "-frames:v",
                "3",
                "-c:v",
                "ffv1",
                "-threads",
                "1",
            ])
            .arg(&source);
        run(command, "generate FFV1 fixture").await.unwrap();
        let source_info = probe(source.to_str().unwrap(), Count::Decoded)
            .await
            .unwrap();
        let mut reference = info();
        reference.fps = 24000.0 / 1001.0;
        let outcome = run(
            comparison_command(source.to_str().unwrap(), &output, &source_info, &reference),
            "retime FFV1",
        )
        .await;
        if outcome.is_ok() {
            let retimed = probe(output.to_str().unwrap(), Count::Packets)
                .await
                .unwrap();
            validate_pair(&source_info, &retimed).unwrap();
            assert!((retimed.fps - reference.fps).abs() < 1e-6);
            let mut hashes = Vec::new();
            for path in [&source, &output] {
                let mut command = Command::new("ffmpeg");
                captured(&mut command)
                    .args(["-v", "error", "-nostdin", "-i"])
                    .arg(path)
                    .args(["-map", "0:v:0", "-f", "framemd5", "-"]);
                let data = run(command, "hash decoded frames").await.unwrap();
                hashes.push(
                    String::from_utf8(data.stdout)
                        .unwrap()
                        .lines()
                        .filter(|l| !l.starts_with('#'))
                        .map(|l| l.rsplit(',').next().unwrap().trim().to_owned())
                        .collect::<Vec<_>>(),
                );
            }
            assert_eq!(hashes[0].len(), 3);
            assert_eq!(hashes[0], hashes[1]);
        }
        tokio::fs::remove_dir_all(&dir).await.unwrap();
        outcome.unwrap();
    }

    #[test]
    fn cvvdp_current_lowercase_csv_header() {
        assert_eq!(
            parse_cvvdp_csv("test, reference, cvvdp\ntest.hevc, ref.mkv, 8.7654321\n").unwrap(),
            8.7654321
        );
    }

    fn png_fixture(depth: u8) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&64u32.to_be_bytes());
        bytes.extend_from_slice(&48u32.to_be_bytes());
        bytes.extend_from_slice(&[depth, 2, 0, 0, 0]);
        // The stream splitter leaves CRC validation/decoding to libjxl.
        bytes.extend_from_slice(&[0; 4]);
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(b"IEND");
        bytes.extend_from_slice(&[0; 4]);
        bytes
    }

    #[tokio::test]
    async fn png_stream_reads_one_frame_at_a_time_and_requires_rgb16() {
        let png = png_fixture(16);
        let pair = [png.clone(), png.clone()].concat();
        let mut stream = pair.as_slice();
        assert_eq!(read_png(&mut stream, &info()).await.unwrap().unwrap(), png);
        assert_eq!(read_png(&mut stream, &info()).await.unwrap().unwrap(), png);
        assert!(read_png(&mut stream, &info()).await.unwrap().is_none());
        assert!(read_png(&mut png_fixture(8).as_slice(), &info())
            .await
            .is_err());
        let mut wrong_dimensions = info();
        wrong_dimensions.width = 32;
        assert!(read_png(&mut png.as_slice(), &wrong_dimensions)
            .await
            .is_err());
        let mut oversized = b"\x89PNG\r\n\x1a\n".to_vec();
        oversized.extend_from_slice(&u32::MAX.to_be_bytes());
        oversized.extend_from_slice(b"IDAT");
        assert!(read_png(&mut oversized.as_slice(), &info()).await.is_err());
    }

    #[tokio::test]
    async fn png_stream_rejects_truncation_and_invalid_signature() {
        assert!(read_png(&mut b"".as_slice(), &info())
            .await
            .unwrap()
            .is_none());
        for bytes in [b"oops".as_slice(), b"not png!", b"\x89PNG\r\n\x1a\n\0"] {
            assert!(read_png(&mut &bytes[..], &info()).await.is_err());
        }
    }

    #[tokio::test]
    async fn frame_pairs_must_end_together() {
        let png = png_fixture(16);
        let two = [png.clone(), png.clone()].concat();
        let (mut reference, mut distorted) = (two.as_slice(), png.as_slice());
        let first = read_pair(&mut reference, &mut distorted, &info()).await;
        assert_eq!(first.unwrap().unwrap(), (png.clone(), png.clone()));
        assert!(read_pair(&mut reference, &mut distorted, &info())
            .await
            .unwrap_err()
            .contains("different frame counts"));
        let (mut reference, mut distorted) = (b"".as_slice(), b"".as_slice());
        assert!(read_pair(&mut reference, &mut distorted, &info())
            .await
            .unwrap()
            .is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn subprocess_failure_is_not_a_score() {
        let mut command = Command::new("sh");
        captured(&mut command).args(["-c", "echo actionable-error >&2; exit 7"]);
        let error = run(command, "test metric").await.unwrap_err();
        assert!(error.contains("actionable-error"));
        assert!(error.contains('7'));
        assert!(run(
            Command::new("sav1n-nonexistent-metric-test"),
            "missing metric"
        )
        .await
        .unwrap_err()
        .contains("PATH"));
    }

    /// Run e.g. `cargo test real_vmaf_smoke -- --ignored --nocapture`.
    async fn real_metric_smoke(kind: MetricKind) {
        let dir = smoke_dir(&format!("metric-smoke-{}", kind.name()));
        tokio::fs::create_dir(&dir).await.unwrap();
        let reference = dir.join("ref.mkv");
        let degraded = dir.join("degraded.mkv");
        let short = dir.join("short.mkv");
        // 64x64 exceeds VMAF's minimum; 8 frames exercise temporal behavior.
        for (path, frames, filter) in [
            (&reference, "8", "testsrc2=size=64x64:rate=24000/1001"),
            (
                &degraded,
                "8",
                "testsrc2=size=64x64:rate=24000/1001,boxblur=5:2",
            ),
            (&short, "7", "testsrc2=size=64x64:rate=24000/1001"),
        ] {
            let mut command = Command::new("ffmpeg");
            captured(&mut command)
                .args([
                    "-v",
                    "error",
                    "-nostdin",
                    "-f",
                    "lavfi",
                    "-i",
                    filter,
                    "-frames:v",
                    frames,
                    "-c:v",
                    "ffv1",
                    "-threads",
                    "1",
                    "-color_primaries",
                    "bt709",
                    "-color_trc",
                    "bt709",
                    "-colorspace",
                    "bt709",
                    "-color_range",
                    "tv",
                ])
                .arg(path);
            run(command, "generate metric smoke fixture").await.unwrap();
        }
        let reference = reference.to_str().unwrap();
        let degraded = degraded.to_str().unwrap();
        let short = short.to_str().unwrap();
        let same_dir = dir.join("identical");
        let degraded_dir = dir.join("degraded");
        let short_dir = dir.join("short");
        let c = config(kind);
        let prepared = c.prepare_reference(reference, &dir.join("scene")).await;
        let (identical, different, mismatch) = match &prepared {
            Ok(prepared) => tokio::join!(
                c.evaluate(prepared, reference, &same_dir),
                c.evaluate(prepared, degraded, &degraded_dir),
                c.evaluate(prepared, short, &short_dir),
            ),
            Err(e) => (Err(e.clone()), Err(e.clone()), Err(e.clone())),
        };
        tokio::fs::remove_dir_all(&dir).await.unwrap();
        let identical = identical.expect("score identical frames");
        let different = different.expect("score degraded frames");
        assert!(identical.is_finite() && different.is_finite());
        assert!(
            kind.search_score(identical) > kind.search_score(different),
            "{} identical={identical}, degraded={different}",
            kind.name()
        );
        assert!(mismatch.unwrap_err().contains("frame count mismatch"));
    }

    #[tokio::test]
    #[ignore = "requires FFmpeg libvmaf and the fixed VMAF model path"]
    async fn real_vmaf_smoke() {
        real_metric_smoke(MetricKind::Vmaf).await;
    }

    #[tokio::test]
    #[ignore = "requires official cvvdp, Python dependencies, FFmpeg and ffprobe"]
    async fn real_cvvdp_smoke() {
        real_metric_smoke(MetricKind::Cvvdp).await;
    }

    #[tokio::test]
    #[ignore = "requires libjxl butteraugli_main and FFmpeg with zscale and PNG"]
    async fn real_butteraugli_smoke() {
        real_metric_smoke(MetricKind::Butteraugli).await;
    }
}

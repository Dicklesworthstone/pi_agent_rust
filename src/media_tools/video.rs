//! Bounded local-video previews for the read tool.
//!
//! FFmpeg reads only the descriptor admitted by ReadTool. Forced single-file
//! demuxers and an fd/pipe protocol allowlist exclude playlists and remote or
//! pathname-based secondary inputs. The helpers run under the request owner;
//! neither detached readers nor temporary media files outlive cancellation.

use crate::agent_cx::{AgentCommand, AgentCx};
use crate::error::{Error, Result};
use crate::model::{ContentBlock, ImageContent, TextContent, sanitize_media_name};
use crate::tools::ToolOutput;
use base64::Engine as _;
use flate2::write::ZlibEncoder;
use flate2::{Compression, Crc};
use serde::Deserialize;
use serde_json::json;
use std::fmt::Write as _;
use std::fs::{File, Metadata};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

const MAX_SOURCE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_SOURCE_PIXELS: u64 = 16_777_216;
const MAX_DURATION_SECONDS: f64 = 86_400.0;
const MAX_FRAME_INDEX: u64 = 10_000_000;
const MAX_METADATA_BYTES: usize = 64 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const MAX_PNG_BYTES: usize = 4_718_592;
const OPERATION_TIMEOUT: Duration = Duration::from_secs(60);
const THUMB_WIDTH: u32 = 320;
const THUMB_HEIGHT: u32 = 240;
const FRAME_EDGE: u32 = 1200;
const CONTACT_FRAMES: usize = 6;
const MAX_ACTIVE_READS: usize = 2;
const PERMIT_POLL: Duration = Duration::from_millis(20);

static ACTIVE_READS: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Debug, PartialEq)]
pub enum VideoSelector {
    Frame(u64),
    Time(f64),
}

#[derive(Clone, Debug, PartialEq)]
pub struct VideoReadTarget {
    pub path: String,
    pub selector: Option<VideoSelector>,
}

fn failure(code: &str, message: impl std::fmt::Display) -> Error {
    Error::tool("read", format!("[{code}] {message}"))
}

fn selector_error() -> Error {
    failure(
        "VIDEO_SELECTOR",
        "Use :412, :f412 or :frame412 for a zero-based frame, or :01:23.5, \
         :1h5m42s or :1.5 for a timestamp; timestamps must be within 24 hours.",
    )
}

fn demuxer(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?;
    if ["mp4", "mov", "m4v"]
        .iter()
        .any(|candidate| extension.eq_ignore_ascii_case(candidate))
    {
        Some("mov")
    } else if ["mkv", "webm"]
        .iter()
        .any(|candidate| extension.eq_ignore_ascii_case(candidate))
    {
        Some("matroska")
    } else if extension.eq_ignore_ascii_case("avi") {
        Some("avi")
    } else if extension.eq_ignore_ascii_case("wmv") {
        Some("asf")
    } else {
        None
    }
}

/// Pure routing: callers check literal existence only within the read scope.
/// A real filename containing a colon wins over selector interpretation.
pub fn parse_target(path: &str, literal_exists: bool) -> Result<Option<VideoReadTarget>> {
    if demuxer(Path::new(path)).is_some() {
        return Ok(Some(VideoReadTarget {
            path: path.to_owned(),
            selector: None,
        }));
    }
    if literal_exists {
        return Ok(None);
    }
    for (index, character) in path.char_indices() {
        if character == ':' && demuxer(Path::new(&path[..index])).is_some() {
            return Ok(Some(VideoReadTarget {
                path: path[..index].to_owned(),
                selector: Some(parse_selector(&path[index + 1..])?),
            }));
        }
    }
    Ok(None)
}

fn decimal(value: &str) -> Option<f64> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
        || value.bytes().filter(|&byte| byte == b'.').count() > 1
        || !value.bytes().any(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    value.parse::<f64>().ok().filter(|value| value.is_finite())
}

fn parse_selector(value: &str) -> Result<VideoSelector> {
    if value.is_empty() || value.len() > 128 || !value.is_ascii() {
        return Err(selector_error());
    }
    let lower = value.to_ascii_lowercase();
    let frame = lower
        .strip_prefix("frame")
        .or_else(|| lower.strip_prefix('f'))
        .unwrap_or(&lower);
    if !frame.is_empty() && frame.bytes().all(|byte| byte.is_ascii_digit()) {
        let frame = frame.parse::<u64>().map_err(|_| selector_error())?;
        return if frame <= MAX_FRAME_INDEX {
            Ok(VideoSelector::Frame(frame))
        } else {
            Err(failure(
                "VIDEO_SELECTOR",
                format!("Frame index exceeds the {MAX_FRAME_INDEX} frame limit."),
            ))
        };
    }
    let seconds = if lower.contains(':') {
        let parts: Vec<&str> = lower.split(':').collect();
        if !(2..=3).contains(&parts.len())
            || parts[..parts.len() - 1]
                .iter()
                .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
        {
            return Err(selector_error());
        }
        let last = decimal(parts[parts.len() - 1]).ok_or_else(selector_error)?;
        let minutes = decimal(parts[parts.len() - 2]).ok_or_else(selector_error)?;
        if last >= 60.0 || minutes >= 60.0 {
            return Err(selector_error());
        }
        let hours = if parts.len() == 3 {
            decimal(parts[0]).ok_or_else(selector_error)?
        } else {
            0.0
        };
        hours * 3600.0 + minutes * 60.0 + last
    } else if lower.bytes().any(|byte| matches!(byte, b'h' | b'm' | b's')) {
        let mut total = 0.0;
        let mut start = 0;
        let mut previous_rank = 0;
        for (index, byte) in lower.bytes().enumerate() {
            let (rank, multiplier) = match byte {
                b'h' => (1, 3600.0),
                b'm' => (2, 60.0),
                b's' => (3, 1.0),
                _ => continue,
            };
            if rank <= previous_rank {
                return Err(selector_error());
            }
            total += decimal(&lower[start..index]).ok_or_else(selector_error)? * multiplier;
            previous_rank = rank;
            start = index + 1;
        }
        if start != lower.len() {
            return Err(selector_error());
        }
        total
    } else {
        // Bare integers were consumed above: a bare decimal means seconds.
        decimal(&lower).ok_or_else(selector_error)?
    };
    if !seconds.is_finite() || !(0.0..=MAX_DURATION_SECONDS).contains(&seconds) {
        return Err(selector_error());
    }
    Ok(VideoSelector::Time(seconds))
}

struct Permit;

impl Permit {
    fn try_acquire() -> Option<Self> {
        ACTIVE_READS
            .try_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < MAX_ACTIVE_READS).then_some(active + 1)
            })
            .ok()
            .map(|_| Self)
    }

    /// Read calls in one turn run together, so a read beyond the limit waits
    /// for a slot inside its own budget instead of failing.
    async fn acquire(owner: &AgentCx, deadline: &Deadline) -> Result<Self> {
        loop {
            if let Some(permit) = Self::try_acquire() {
                return Ok(permit);
            }
            owner
                .time()
                .sleep(PERMIT_POLL.min(deadline.remaining(owner)?))
                .await;
        }
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        ACTIVE_READS.fetch_sub(1, Ordering::AcqRel);
    }
}

struct Deadline(Instant);

impl Deadline {
    fn new() -> Self {
        Self(Instant::now() + OPERATION_TIMEOUT)
    }

    fn remaining(&self, owner: &AgentCx) -> Result<Duration> {
        owner.checkpoint().map_err(|_| Error::Aborted)?;
        self.0
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| {
                failure(
                    "VIDEO_TIMEOUT",
                    "Video inspection exceeded its 60-second operation budget.",
                )
            })
    }

    fn check(&self, owner: &AgentCx) -> Result<()> {
        self.remaining(owner).map(|_| ())
    }
}

#[derive(Debug)]
struct SourceStamp {
    len: u64,
    modified: Option<SystemTime>,
}

impl SourceStamp {
    fn new(metadata: &Metadata) -> Result<Self> {
        if !metadata.is_file() {
            return Err(failure(
                "VIDEO_SOURCE",
                "Video input must be a regular file.",
            ));
        }
        if metadata.len() == 0 || metadata.len() > MAX_SOURCE_BYTES {
            return Err(failure(
                "VIDEO_SIZE",
                "Video input must contain between 1 byte and 256 MiB.",
            ));
        }
        Ok(Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }

    fn check(&self, file: &File) -> Result<()> {
        let current = file.metadata().map_err(process_error)?;
        if current.len() != self.len || current.modified().ok() != self.modified {
            return Err(failure(
                "VIDEO_CHANGED",
                "The opened video changed during inspection; retry after the writer finishes.",
            ));
        }
        Ok(())
    }
}

fn process_error(error: io::Error) -> Error {
    match error.kind() {
        io::ErrorKind::Interrupted => Error::Aborted,
        io::ErrorKind::TimedOut => failure(
            "VIDEO_TIMEOUT",
            "Video helper exceeded the operation budget.",
        ),
        _ => failure("VIDEO_PROCESS", error),
    }
}

struct Helpers {
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
}

fn find_binary(name: &str, owner: &AgentCx, deadline: &Deadline) -> Result<PathBuf> {
    #[cfg(windows)]
    let filename = format!("{name}.exe");
    #[cfg(not(windows))]
    let filename = name.to_owned();
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path).take(128) {
            deadline.check(owner)?;
            // Empty and relative PATH entries allow workspace helper shadowing.
            if !directory.is_absolute() {
                continue;
            }
            let candidate = directory.join(&filename);
            let Ok(metadata) = candidate.metadata() else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                if metadata.permissions().mode() & 0o111 == 0 {
                    continue;
                }
            }
            return Ok(candidate);
        }
    }
    Err(failure(
        "VIDEO_HELPER_MISSING",
        format!(
            "Install FFmpeg with ffmpeg and ffprobe on an absolute PATH entry; {name} was not found."
        ),
    ))
}

fn helper_command(owner: &AgentCx, executable: &Path) -> AgentCommand {
    let mut command = owner.process().command(executable);
    command.env_clear().stdin(Stdio::null());
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", root);
    }
    command
}

async fn run_helper(
    command: AgentCommand,
    owner: &AgentCx,
    deadline: &Deadline,
    stdout_limit: usize,
) -> Result<Output> {
    deadline.check(owner)?;
    let output = capture(command, owner, deadline, stdout_limit + MAX_STDERR_BYTES).await?;
    deadline.check(owner)?;
    if output.stdout.len() > stdout_limit || output.stderr.len() > MAX_STDERR_BYTES {
        return Err(failure(
            "VIDEO_OUTPUT_LIMIT",
            "Video helper output exceeded its byte limit.",
        ));
    }
    if !output.status.success() {
        let diagnostic = sanitize_media_name(&String::from_utf8_lossy(&output.stderr))
            .unwrap_or_else(|| "No decoder diagnostic was provided.".to_owned());
        return Err(failure(
            "VIDEO_DECODE",
            format!("Video helper exited with {}: {diagnostic}", output.status),
        ));
    }
    Ok(output)
}

async fn capture(
    mut command: AgentCommand,
    owner: &AgentCx,
    deadline: &Deadline,
    byte_limit: usize,
) -> Result<Output> {
    deadline.check(owner)?;
    #[cfg(unix)]
    {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let child = command.spawn().map_err(process_error)?;
        // In particular, retain no parent copies of custom Windows writers.
        drop(command);
        child
            .wait_with_output_limited(byte_limit, deadline.remaining(owner)?)
            .await
            .map_err(process_error)
    }
    #[cfg(windows)]
    {
        use crate::jobs::windows_job_pipe::pair;

        let (stdout, stdout_writer) = pair().map_err(process_error)?;
        let (stderr, stderr_writer) = pair().map_err(process_error)?;
        command
            .stdout(Stdio::from(stdout_writer))
            .stderr(Stdio::from(stderr_writer));
        let mut child = command.spawn().map_err(process_error)?;
        drop(command);
        let mut stdout_pipe = Some(stdout);
        let mut stderr_pipe = Some(stderr);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut remaining = byte_limit;
        loop {
            deadline.check(owner)?;
            drain_windows_pipe(&mut stdout_pipe, &mut stdout, &mut remaining)?;
            drain_windows_pipe(&mut stderr_pipe, &mut stderr, &mut remaining)?;
            deadline.check(owner)?;
            if let Some(status) = child.try_wait().map_err(process_error)?
                && stdout_pipe.is_none()
                && stderr_pipe.is_none()
            {
                return Ok(Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            owner
                .time()
                .sleep(Duration::from_millis(2).min(deadline.remaining(owner)?))
                .await;
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (command, byte_limit);
        Err(failure(
            "VIDEO_PLATFORM",
            "Video previews require Unix or Windows.",
        ))
    }
}

#[cfg(windows)]
fn drain_windows_pipe(
    pipe: &mut Option<crate::jobs::windows_job_pipe::Reader>,
    bytes: &mut Vec<u8>,
    remaining: &mut usize,
) -> Result<()> {
    let Some(reader) = pipe.as_mut() else {
        return Ok(());
    };
    let mut buffer = [0_u8; 8192];
    // Bound work per stream so noisy stderr cannot starve cancellation/stdout.
    for _ in 0..8 {
        let capacity = remaining.saturating_add(1).min(buffer.len());
        match reader.try_read(&mut buffer[..capacity]) {
            Ok(0) => {
                drop(pipe.take());
                return Ok(());
            }
            Ok(count) => {
                if count > *remaining {
                    return Err(failure(
                        "VIDEO_OUTPUT_LIMIT",
                        "Video helper output exceeded its combined byte limit.",
                    ));
                }
                bytes.extend_from_slice(&buffer[..count]);
                *remaining -= count;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(process_error(error)),
        }
    }
    Ok(())
}

fn has_fd_input_protocol(output: &[u8]) -> bool {
    let mut input = false;
    for line in String::from_utf8_lossy(output).lines() {
        match line.trim() {
            "Input:" => input = true,
            "Output:" => input = false,
            "fd" if input => return true,
            _ => {}
        }
    }
    false
}

async fn check_helper(executable: &Path, owner: &AgentCx, deadline: &Deadline) -> Result<()> {
    let mut command = helper_command(owner, executable);
    command.args(["-hide_banner", "-protocols"]);
    let output = run_helper(command, owner, deadline, MAX_METADATA_BYTES).await?;
    if !has_fd_input_protocol(&output.stdout) {
        return Err(failure(
            "VIDEO_FD_PROTOCOL",
            "Video previews require ffmpeg and ffprobe builds with the seekable fd input \
             protocol (FFmpeg 6 or newer). Upgrade FFmpeg; pathname fallback is disabled.",
        ));
    }
    Ok(())
}

fn input_options(command: &mut AgentCommand, demuxer: &str) {
    command.args([
        "-max_alloc",
        "67108864",
        "-protocol_whitelist",
        "fd,pipe",
        "-probesize",
        "8388608",
        "-analyzeduration",
        "5000000",
        "-max_pixels",
        "16777216",
        "-threads",
        "1",
        "-f",
        demuxer,
    ]);
    if demuxer == "mov" {
        command.args(["-enable_drefs", "0", "-use_absolute_path", "0"]);
    }
}

fn attach_source(command: &mut AgentCommand, file: &mut File, stamp: &SourceStamp) -> Result<()> {
    stamp.check(file)?;
    // Duplicated handles share the file cursor. All subprocesses are sequential.
    file.seek(SeekFrom::Start(0)).map_err(process_error)?;
    command
        .stdin(Stdio::from(file.try_clone().map_err(process_error)?))
        .args(["-i", "fd:"]);
    Ok(())
}

#[derive(Deserialize)]
struct Probe {
    #[serde(default)]
    streams: Vec<ProbeStream>,
    format: Option<ProbeFormat>,
}

#[derive(Deserialize)]
struct ProbeStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    duration: Option<String>,
    avg_frame_rate: Option<String>,
    r_frame_rate: Option<String>,
    nb_frames: Option<String>,
    #[serde(default)]
    disposition: ProbeDisposition,
}

#[derive(Default, Deserialize)]
struct ProbeDisposition {
    #[serde(default)]
    attached_pic: u8,
}

#[derive(Deserialize)]
struct ProbeFormat {
    duration: Option<String>,
}

#[derive(Debug)]
struct VideoInfo {
    width: u32,
    height: u32,
    duration: Option<f64>,
    fps: Option<f64>,
    frame_count: Option<u64>,
    codec: String,
    audio_codec: Option<String>,
}

fn probe_duration(value: Option<&str>) -> Result<Option<f64>> {
    match value {
        None | Some("" | "N/A") => Ok(None),
        Some(value) => {
            let seconds = value
                .parse::<f64>()
                .ok()
                .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
                .ok_or_else(|| failure("VIDEO_METADATA", "Invalid video duration."))?;
            if seconds > MAX_DURATION_SECONDS {
                return Err(failure(
                    "VIDEO_DURATION",
                    "Video duration exceeds the 24-hour limit.",
                ));
            }
            Ok((seconds > 0.0).then_some(seconds))
        }
    }
}

fn frame_rate(value: Option<&str>) -> Option<f64> {
    let value = value?;
    let rate = if let Some((numerator, denominator)) = value.split_once('/') {
        let numerator = numerator.parse::<f64>().ok()?;
        let denominator = denominator.parse::<f64>().ok()?;
        numerator / denominator
    } else {
        value.parse::<f64>().ok()?
    };
    (rate.is_finite() && rate > 0.0 && rate <= 1000.0).then_some(rate)
}

fn parse_probe(bytes: &[u8]) -> Result<VideoInfo> {
    let probe: Probe = serde_json::from_slice(bytes)
        .map_err(|_| failure("VIDEO_METADATA", "ffprobe returned invalid video metadata."))?;
    if probe.streams.len() > 64 {
        return Err(failure(
            "VIDEO_STREAMS",
            "Video contains more than 64 streams.",
        ));
    }
    let video = probe
        .streams
        .iter()
        .find(|stream| {
            stream.codec_type.as_deref() == Some("video") && stream.disposition.attached_pic == 0
        })
        .ok_or_else(|| {
            failure(
                "VIDEO_STREAM",
                "The file contains no non-attached video stream.",
            )
        })?;
    let width = video.width.unwrap_or(0);
    let height = video.height.unwrap_or(0);
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_SOURCE_PIXELS {
        return Err(failure(
            "VIDEO_DIMENSIONS",
            "Video dimensions are missing or exceed the 16-megapixel decoded-frame limit.",
        ));
    }
    let duration = probe_duration(video.duration.as_deref())?.or(probe_duration(
        probe
            .format
            .as_ref()
            .and_then(|format| format.duration.as_deref()),
    )?);
    let codec = video
        .codec_name
        .as_deref()
        .and_then(sanitize_media_name)
        .unwrap_or_else(|| "unknown".to_owned());
    let audio_codec = probe
        .streams
        .iter()
        .find(|stream| stream.codec_type.as_deref() == Some("audio"))
        .and_then(|stream| stream.codec_name.as_deref())
        .and_then(sanitize_media_name);
    Ok(VideoInfo {
        width,
        height,
        duration,
        fps: frame_rate(video.avg_frame_rate.as_deref())
            .or_else(|| frame_rate(video.r_frame_rate.as_deref())),
        frame_count: video
            .nb_frames
            .as_deref()
            .and_then(|frames| frames.parse().ok())
            .filter(|&frames| frames > 0),
        codec,
        audio_codec,
    })
}

async fn probe_video(
    file: &mut File,
    stamp: &SourceStamp,
    demuxer: &str,
    helpers: &Helpers,
    owner: &AgentCx,
    deadline: &Deadline,
) -> Result<VideoInfo> {
    deadline.check(owner)?;
    let mut command = helper_command(owner, &helpers.ffprobe);
    command.args(["-hide_banner", "-v", "error"]);
    input_options(&mut command, demuxer);
    command.args([
        "-show_entries",
        "stream=codec_type,codec_name,width,height,duration,avg_frame_rate,r_frame_rate,nb_frames:stream_disposition=attached_pic:format=duration",
        "-of",
        "json",
    ]);
    attach_source(&mut command, file, stamp)?;
    let output = run_helper(command, owner, deadline, MAX_METADATA_BYTES).await?;
    parse_probe(&output.stdout)
}

fn validate_selection(selector: &VideoSelector, info: &VideoInfo) -> Result<()> {
    match selector {
        VideoSelector::Frame(index) => {
            if *index > MAX_FRAME_INDEX || info.frame_count.is_some_and(|frames| *index >= frames) {
                return Err(failure(
                    "VIDEO_FRAME_RANGE",
                    "Requested frame is outside the video or frame limit.",
                ));
            }
        }
        VideoSelector::Time(seconds) => {
            if !seconds.is_finite()
                || !(0.0..=MAX_DURATION_SECONDS).contains(seconds)
                || info.duration.is_some_and(|duration| *seconds >= duration)
            {
                return Err(failure(
                    "VIDEO_TIME_RANGE",
                    "Requested timestamp is outside the video or time limit.",
                ));
            }
        }
    }
    Ok(())
}

#[allow(clippy::cast_precision_loss)] // The indices and divisor are bounded by six.
fn sample_times(info: &VideoInfo) -> Vec<f64> {
    let Some(duration) = info.duration else {
        return vec![0.0];
    };
    // Leave a frame interval before the end when fps is known. Short clips
    // produce fewer distinct samples rather than six copies of the same frame.
    let horizon = (duration - info.fps.map_or(duration * 0.05, |fps| 1.0 / fps)).max(0.0);
    let mut times = Vec::with_capacity(CONTACT_FRAMES);
    for index in 0..CONTACT_FRAMES {
        let time = horizon * index as f64 / (CONTACT_FRAMES - 1) as f64;
        if times
            .last()
            .is_none_or(|previous: &f64| (time - previous).abs() >= 0.001)
        {
            times.push(time);
        }
    }
    times
}

fn raw_size(width: u32, height: u32) -> Result<usize> {
    let bytes = u64::from(width) * u64::from(height) * 3;
    if width == 0 || height == 0 || bytes > u64::from(FRAME_EDGE) * u64::from(FRAME_EDGE) * 3 {
        return Err(failure(
            "VIDEO_OUTPUT_LIMIT",
            "Requested preview dimensions exceed the image limit.",
        ));
    }
    usize::try_from(bytes)
        .map_err(|_| failure("VIDEO_OUTPUT_LIMIT", "Preview size does not fit memory."))
}

// Explicit inputs keep the admitted file and operation owner visible at every decode.
#[allow(clippy::too_many_arguments)]
async fn extract_frame(
    file: &mut File,
    stamp: &SourceStamp,
    demuxer: &str,
    executable: &Path,
    selector: &VideoSelector,
    dimensions: (u32, u32),
    owner: &AgentCx,
    deadline: &Deadline,
) -> Result<Option<Vec<u8>>> {
    deadline.check(owner)?;
    let (width, height) = dimensions;
    let expected = raw_size(width, height)?;
    let mut command = helper_command(owner, executable);
    command.args([
        "-hide_banner",
        "-v",
        "error",
        "-nostdin",
        "-filter_threads",
        "1",
    ]);
    input_options(&mut command, demuxer);
    if let VideoSelector::Time(seconds) = selector {
        command.args(["-ss", &format!("{seconds:.6}")]);
    }
    attach_source(&mut command, file, stamp)?;
    let scale = format!(
        "scale={width}:{height}:force_original_aspect_ratio=decrease,\
         pad={width}:{height}:(ow-iw)/2:(oh-ih)/2,setsar=1"
    );
    let filter = match selector {
        // Deriving a timestamp from average fps is incorrect for VFR video.
        VideoSelector::Frame(index) => format!("select=eq(n\\,{index}),{scale}"),
        VideoSelector::Time(_) => scale,
    };
    command.args([
        "-map",
        "0:V:0",
        "-an",
        "-sn",
        "-dn",
        "-frames:v",
        "1",
        "-vf",
        &filter,
        "-fps_mode",
        "passthrough",
        "-threads",
        "1",
        "-pix_fmt",
        "rgb24",
        "-f",
        "rawvideo",
        "pipe:1",
    ]);
    let output = run_helper(command, owner, deadline, expected).await?;
    stamp.check(file)?;
    if output.stdout.is_empty() {
        return Ok(None);
    }
    if output.stdout.len() != expected {
        return Err(failure(
            "VIDEO_FRAME",
            "Decoder returned an incomplete or incorrectly sized RGB frame.",
        ));
    }
    Ok(Some(output.stdout))
}

fn compose_sheet(frames: &[Vec<u8>]) -> Result<(u32, u32, Vec<u8>)> {
    if frames.is_empty() || frames.len() > CONTACT_FRAMES {
        return Err(failure(
            "VIDEO_FRAME",
            "No usable video frames were decoded.",
        ));
    }
    let columns = u32::try_from(frames.len().min(3)).unwrap_or(3);
    let rows = u32::try_from(frames.len().div_ceil(3)).unwrap_or(2);
    let width = columns * THUMB_WIDTH;
    let height = rows * THUMB_HEIGHT;
    let mut rgb = vec![0_u8; raw_size(width, height)?];
    let thumb_size = raw_size(THUMB_WIDTH, THUMB_HEIGHT)?;
    let row_bytes = usize::try_from(THUMB_WIDTH * 3).unwrap_or(960);
    let sheet_row_bytes = usize::try_from(width * 3).unwrap_or(2880);
    for (index, frame) in frames.iter().enumerate() {
        if frame.len() != thumb_size {
            return Err(failure(
                "VIDEO_FRAME",
                "Contact-sheet frame has an invalid size.",
            ));
        }
        let column = index % columns as usize;
        let row = index / columns as usize;
        for y in 0..THUMB_HEIGHT as usize {
            let source = y * row_bytes;
            let target = (row * THUMB_HEIGHT as usize + y) * sheet_row_bytes + column * row_bytes;
            rgb[target..target + row_bytes].copy_from_slice(&frame[source..source + row_bytes]);
        }
    }
    Ok((width, height, rgb))
}

fn png_chunk(png: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) -> Result<()> {
    let length = u32::try_from(data.len())
        .map_err(|_| failure("VIDEO_OUTPUT_LIMIT", "PNG chunk exceeds its size limit."))?;
    png.extend_from_slice(&length.to_be_bytes());
    png.extend_from_slice(&kind);
    png.extend_from_slice(data);
    let mut crc = Crc::new();
    crc.update(&kind);
    crc.update(data);
    png.extend_from_slice(&crc.sum().to_be_bytes());
    Ok(())
}

/// Encode only our controlled RGB buffer; no untrusted image parser is needed.
/// PNG uses zlib (including Adler-32) inside IDAT and CRC-32 for every chunk.
fn encode_png(width: u32, height: u32, rgb: &[u8]) -> Result<Vec<u8>> {
    if rgb.len() != raw_size(width, height)? {
        return Err(failure(
            "VIDEO_FRAME",
            "RGB preview length does not match its dimensions.",
        ));
    }
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    let row_bytes = usize::try_from(width * 3)
        .map_err(|_| failure("VIDEO_OUTPUT_LIMIT", "PNG row size is invalid."))?;
    for row in rgb.chunks_exact(row_bytes) {
        encoder.write_all(&[0]).map_err(process_error)?;
        encoder.write_all(row).map_err(process_error)?;
    }
    let compressed = encoder.finish().map_err(process_error)?;
    let mut png = Vec::with_capacity(compressed.len() + 57);
    png.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    let mut header = [0_u8; 13];
    header[..4].copy_from_slice(&width.to_be_bytes());
    header[4..8].copy_from_slice(&height.to_be_bytes());
    header[8] = 8; // Eight bits per sample.
    header[9] = 2; // Truecolor RGB.
    png_chunk(&mut png, *b"IHDR", &header)?;
    png_chunk(&mut png, *b"IDAT", &compressed)?;
    png_chunk(&mut png, *b"IEND", &[])?;
    if png.len() > MAX_PNG_BYTES {
        return Err(failure(
            "VIDEO_OUTPUT_LIMIT",
            "Encoded preview exceeds the image byte limit.",
        ));
    }
    Ok(png)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Validated finite 0..=86400 seconds.
fn display_time(seconds: f64) -> String {
    let milliseconds = (seconds * 1000.0).round() as u64;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        milliseconds / 3_600_000,
        milliseconds / 60_000 % 60,
        milliseconds / 1000 % 60,
        milliseconds % 1000
    )
}

/// Render an already-opened, scope-admitted local file. The descriptor prevents
/// pathname replacement from redirecting reads. Concurrent in-place mutations
/// are detected through length/mtime checks, not claimed to be an immutable
/// snapshot. Decoder allocation limits are per allocation, not a total RSS cap.
#[allow(clippy::too_many_lines)] // One lifetime owns the permit, descriptor, deadline, and helpers.
pub async fn render_open_file(
    mut file: File,
    path: PathBuf,
    selector: Option<VideoSelector>,
    owner: AgentCx,
) -> Result<ToolOutput> {
    let capabilities = owner.capabilities();
    if !capabilities.io || !capabilities.spawn || !capabilities.time {
        return Err(failure(
            "VIDEO_CAPABILITY",
            "Video previews require filesystem, process-spawn, and timer capabilities.",
        ));
    }
    let deadline = Deadline::new();
    deadline.check(&owner)?;
    let _permit = Permit::acquire(&owner, &deadline).await?;
    let stamp = SourceStamp::new(&file.metadata().map_err(process_error)?)?;
    let demuxer = demuxer(&path)
        .ok_or_else(|| failure("VIDEO_FORMAT", "Unsupported video container extension."))?;
    let helpers = Helpers {
        ffmpeg: find_binary("ffmpeg", &owner, &deadline)?,
        ffprobe: find_binary("ffprobe", &owner, &deadline)?,
    };
    // Test the precise capability, not just a version string or executable name.
    check_helper(&helpers.ffmpeg, &owner, &deadline).await?;
    check_helper(&helpers.ffprobe, &owner, &deadline).await?;
    let info = probe_video(&mut file, &stamp, demuxer, &helpers, &owner, &deadline).await?;
    if let Some(selector) = selector.as_ref() {
        validate_selection(selector, &info)?;
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(sanitize_media_name)
        .unwrap_or_else(|| "video".to_owned());
    let duration = info
        .duration
        .map_or_else(|| "unknown".to_owned(), display_time);
    let fps = info
        .fps
        .map_or_else(|| "unknown".to_owned(), |fps| format!("{fps:.3}"));
    let audio = info.audio_codec.as_deref().unwrap_or("none");
    let mut description = format!(
        "Video: {name}\nVideo stream: {}, {}x{}; {fps} fps\n\
         Duration: {duration}; source: {} bytes; audio: {audio}\n",
        info.codec, info.width, info.height, stamp.len
    );
    let mut timestamps = Vec::new();
    let (width, height, rgb, rendered_count) = if let Some(selection) = selector.as_ref() {
        let frame = extract_frame(
            &mut file,
            &stamp,
            demuxer,
            &helpers.ffmpeg,
            selection,
            (FRAME_EDGE, FRAME_EDGE),
            &owner,
            &deadline,
        )
        .await?
        .ok_or_else(|| {
            failure(
                "VIDEO_EMPTY_FRAME",
                "No frame exists at the requested position.",
            )
        })?;
        match selection {
            VideoSelector::Frame(index) => {
                let _ = writeln!(
                    description,
                    "Selected frame: {index} (zero-based decoded frame index)."
                );
            }
            VideoSelector::Time(seconds) => {
                timestamps.push(*seconds);
                let _ = writeln!(
                    description,
                    "Requested timestamp: {} (first available frame at or after this position).",
                    display_time(*seconds)
                );
            }
        }
        (FRAME_EDGE, FRAME_EDGE, frame, 1)
    } else {
        let requested_times = sample_times(&info);
        let requested_count = requested_times.len();
        let mut frames = Vec::with_capacity(requested_count);
        for time in requested_times {
            if let Some(frame) = extract_frame(
                &mut file,
                &stamp,
                demuxer,
                &helpers.ffmpeg,
                &VideoSelector::Time(time),
                (THUMB_WIDTH, THUMB_HEIGHT),
                &owner,
                &deadline,
            )
            .await?
            {
                timestamps.push(time);
                frames.push(frame);
            }
        }
        let (width, height, rgb) = compose_sheet(&frames)?;
        let _ = writeln!(
            description,
            "Contact sheet: {} frames, {} columns x {} rows; read left-to-right, then top-to-bottom.\n\
             Requested timestamps: {}.",
            frames.len(),
            frames.len().min(3),
            frames.len().div_ceil(3),
            timestamps
                .iter()
                .copied()
                .map(display_time)
                .collect::<Vec<_>>()
                .join(", ")
        );
        if info.duration.is_none() {
            description
                .push_str("Duration is unknown; the preview contains the first available frame.\n");
        }
        if frames.len() < requested_count {
            description
                .push_str("Some requested positions had no decodable frame and were omitted.\n");
        }
        description.push_str(
            "Timestamp previews use the first available frame at or after each requested position. \
             Append :f412 for a precise zero-based frame, or :01:23.5 for a timestamp.\n",
        );
        (width, height, rgb, frames.len())
    };
    deadline.check(&owner)?;
    stamp.check(&file)?;
    let png = encode_png(width, height, &rgb)?;
    deadline.check(&owner)?;
    stamp.check(&file)?;
    let selected_frame = match selector.as_ref() {
        Some(VideoSelector::Frame(index)) => Some(*index),
        _ => None,
    };
    Ok(ToolOutput {
        content: vec![
            ContentBlock::Text(TextContent::new(description)),
            ContentBlock::Image(ImageContent {
                data: base64::engine::general_purpose::STANDARD.encode(png),
                mime_type: "image/png".to_owned(),
            }),
        ],
        details: Some(json!({
            "video": {
                "name": name,
                "sourceBytes": stamp.len,
                "width": info.width,
                "height": info.height,
                "durationSeconds": info.duration,
                "framesPerSecond": info.fps,
                "frameCount": info.frame_count,
                "videoCodec": info.codec,
                "audioCodec": info.audio_codec,
                "selectedFrame": selected_frame,
                "requestedTimestamps": timestamps,
                "renderedFrames": rendered_count,
                "previewWidth": width,
                "previewHeight": height
            }
        })),
        is_error: false,
    })
}

#[cfg(test)]
#[allow(clippy::float_cmp, clippy::cast_possible_truncation)] // Exact parser values and tiny fixtures.
mod tests {
    use super::*;
    use flate2::read::ZlibDecoder;
    use std::io::Read as _;

    #[test]
    fn routes_video_selectors_without_stealing_literal_colon_filenames() {
        for extension in ["mp4", "MOV", "mkv", "webm", "m4v", "avi", "wmv"] {
            let path = format!("clip.{extension}");
            assert_eq!(
                parse_target(&path, false).unwrap(),
                Some(VideoReadTarget {
                    path,
                    selector: None
                })
            );
        }
        assert_eq!(parse_target("literal.mp4:12", true).unwrap(), None);
        assert_eq!(parse_target("notes.txt:12", false).unwrap(), None);
        for suffix in ["12", "f12", "frame12", "FRAME12"] {
            let target = parse_target(&format!("C:\\clips\\demo.mp4:{suffix}"), false)
                .unwrap()
                .unwrap();
            assert_eq!(target.path, "C:\\clips\\demo.mp4");
            assert_eq!(target.selector, Some(VideoSelector::Frame(12)));
        }
        for (suffix, seconds) in [
            ("01:23.5", 83.5),
            ("1:05:42", 3942.0),
            ("1h5m42s", 3942.0),
            ("1.5s", 1.5),
            ("1.5", 1.5),
            ("0.0", 0.0),
        ] {
            assert_eq!(
                parse_target(&format!("clip.mp4:{suffix}"), false)
                    .unwrap()
                    .unwrap()
                    .selector,
                Some(VideoSelector::Time(seconds))
            );
        }
    }

    #[test]
    fn rejects_ambiguous_or_unbounded_selector_expressions() {
        for suffix in [
            "",
            "-1",
            "+1",
            "NaN",
            "inf",
            "1e4",
            "1 s",
            "1s1m",
            "1h2h",
            "1.2.3",
            "1:60",
            "60:01",
            "25h",
            "f1.5",
            "10000001",
            "184467440737095516160",
            "frame1s",
            "1:2:3:4",
        ] {
            let error = parse_target(&format!("clip.mp4:{suffix}"), false).unwrap_err();
            assert!(
                error.to_string().contains("VIDEO_SELECTOR"),
                "{suffix}: {error}"
            );
        }
    }

    fn info() -> VideoInfo {
        parse_probe(
            br#"{"streams":[{"codec_type":"audio","codec_name":"aac"},
                {"codec_type":"video","codec_name":"h264","width":1920,"height":1080,
                 "avg_frame_rate":"30000/1001","nb_frames":"300","duration":"10.01"}],
                "format":{"duration":"10.01"}}"#,
        )
        .unwrap()
    }

    #[test]
    fn metadata_bounds_and_exact_selection_do_not_assume_constant_frame_rate() {
        let info = info();
        assert_eq!(info.audio_codec.as_deref(), Some("aac"));
        assert!((info.fps.unwrap() - 29.970_029_97).abs() < 0.000_001);
        assert!(validate_selection(&VideoSelector::Frame(299), &info).is_ok());
        assert!(validate_selection(&VideoSelector::Frame(300), &info).is_err());
        assert!(validate_selection(&VideoSelector::Time(10.01), &info).is_err());
        assert!(validate_selection(&VideoSelector::Time(f64::NAN), &info).is_err());
        for metadata in [
            br#"{"streams":[{"codec_type":"audio"}]}"#.as_slice(),
            br#"{"streams":[{"codec_type":"video","width":100000,"height":100000}]}"#,
            br#"{"streams":[{"codec_type":"video","width":1,"height":1,"duration":"NaN"}]}"#,
            br#"{"streams":[{"codec_type":"video","width":1,"height":1,"duration":"86401"}]}"#,
        ] {
            assert!(parse_probe(metadata).is_err());
        }
        assert!(!has_fd_input_protocol(b"Input:\nfile\nOutput:\nfd\n"));
        assert!(has_fd_input_protocol(
            b"Supported file protocols:\nInput:\n  fd\nOutput:\npipe\n"
        ));
    }

    #[test]
    fn metadata_skips_attached_cover_art_and_preserves_audio() {
        let metadata = br#"{"streams":[
            {"codec_type":"video","codec_name":"mjpeg","width":640,"height":640,
             "disposition":{"attached_pic":1}},
            {"codec_type":"audio","codec_name":"aac"},
            {"codec_type":"video","codec_name":"h264","width":1920,"height":1080,
             "duration":"5.0","disposition":{"attached_pic":0}}
        ]}"#;
        let video = parse_probe(metadata).unwrap();
        assert_eq!((video.width, video.height), (1920, 1080));
        assert_eq!(video.codec, "h264");
        assert_eq!(video.audio_codec.as_deref(), Some("aac"));
        let cover_only = br#"{"streams":[
            {"codec_type":"audio","codec_name":"aac"},
            {"codec_type":"video","codec_name":"mjpeg","width":640,"height":640,
             "disposition":{"attached_pic":1}}
        ]}"#;
        let error = parse_probe(cover_only).unwrap_err();
        assert!(error.to_string().contains("VIDEO_STREAM"));
    }

    #[test]
    fn contact_samples_fit_short_and_unknown_duration_videos() {
        let mut info = info();
        let times = sample_times(&info);
        assert_eq!(times.len(), CONTACT_FRAMES);
        assert_eq!(times[0], 0.0);
        assert!(times.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(times.last().unwrap() < &info.duration.unwrap());
        info.duration = Some(0.01);
        assert_eq!(sample_times(&info), vec![0.0]);
        info.duration = None;
        assert_eq!(sample_times(&info), vec![0.0]);
    }

    fn decode_our_png(png: &[u8]) -> (u32, u32, Vec<u8>) {
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let mut offset = 8;
        let mut compressed = Vec::new();
        let mut dimensions = (0, 0);
        let mut saw_end = false;
        while offset < png.len() {
            let length = u32::from_be_bytes(png[offset..offset + 4].try_into().unwrap()) as usize;
            let kind = &png[offset + 4..offset + 8];
            let data = &png[offset + 8..offset + 8 + length];
            let stored_crc = u32::from_be_bytes(
                png[offset + 8 + length..offset + 12 + length]
                    .try_into()
                    .unwrap(),
            );
            let mut crc = Crc::new();
            crc.update(kind);
            crc.update(data);
            assert_eq!(crc.sum(), stored_crc);
            match kind {
                b"IHDR" => {
                    dimensions = (
                        u32::from_be_bytes(data[..4].try_into().unwrap()),
                        u32::from_be_bytes(data[4..8].try_into().unwrap()),
                    );
                    assert_eq!(&data[8..], &[8, 2, 0, 0, 0]);
                }
                b"IDAT" => compressed.extend_from_slice(data),
                b"IEND" => saw_end = true,
                _ => panic!("unexpected PNG chunk"),
            }
            offset += 12 + length;
        }
        assert!(saw_end);
        let mut filtered = Vec::new();
        ZlibDecoder::new(compressed.as_slice())
            .read_to_end(&mut filtered)
            .unwrap();
        let stride = dimensions.0 as usize * 3 + 1;
        assert_eq!(filtered.len(), stride * dimensions.1 as usize);
        let mut rgb = Vec::new();
        for row in filtered.chunks_exact(stride) {
            assert_eq!(row[0], 0);
            rgb.extend_from_slice(&row[1..]);
        }
        (dimensions.0, dimensions.1, rgb)
    }

    #[test]
    fn contact_sheet_order_png_crc_and_zlib_round_trip() {
        let mut crc = Crc::new();
        crc.update(b"123456789");
        assert_eq!(crc.sum(), 0xcbf4_3926); // PNG's standard IEEE CRC-32.
        let frames: Vec<Vec<u8>> = (1..=6)
            .map(|color| vec![color; raw_size(THUMB_WIDTH, THUMB_HEIGHT).unwrap()])
            .collect();
        let (width, height, rgb) = compose_sheet(&frames).unwrap();
        assert_eq!((width, height), (960, 480));
        for (index, expected) in (1..=6).enumerate() {
            let offset = ((index / 3 * THUMB_HEIGHT as usize) * width as usize
                + index % 3 * THUMB_WIDTH as usize)
                * 3;
            assert_eq!(rgb[offset], expected);
        }
        let png = encode_png(width, height, &rgb).unwrap();
        assert_eq!(decode_our_png(&png), (width, height, rgb));
        assert!(encode_png(1, 1, &[0, 0]).is_err());
        assert!(raw_size(1201, 1200).is_err());
        assert!(compose_sheet(&[vec![0]]).is_err());
    }

    #[test]
    fn regular_source_limit_and_mutation_check_precede_decoder_work() {
        let mut file = tempfile::tempfile().unwrap();
        assert!(SourceStamp::new(&file.metadata().unwrap()).is_err());
        file.write_all(b"video").unwrap();
        let stamp = SourceStamp::new(&file.metadata().unwrap()).unwrap();
        file.set_len(10).unwrap();
        assert!(
            stamp
                .check(&file)
                .unwrap_err()
                .to_string()
                .contains("VIDEO_CHANGED")
        );
        file.set_len(MAX_SOURCE_BYTES + 1).unwrap();
        assert!(
            SourceStamp::new(&file.metadata().unwrap())
                .unwrap_err()
                .to_string()
                .contains("VIDEO_SIZE")
        );
    }

    #[test]
    fn cancelled_owner_is_checked_before_helpers_or_source_metadata() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        let owner = AgentCx::for_request();
        owner.cancel_with(asupersync::types::CancelKind::User, Some("video cancelled"));
        let result = runtime.block_on(render_open_file(
            tempfile::tempfile().unwrap(),
            PathBuf::from("missing.mp4"),
            None,
            owner,
        ));
        assert!(matches!(result, Err(Error::Aborted)));
    }

    #[cfg(unix)]
    #[test]
    fn subprocess_reads_pinned_descriptor_after_path_replacement_and_rewinds() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.mp4");
        let replacement = directory.path().join("replacement.mp4");
        std::fs::write(&path, b"original admitted bytes\n").unwrap();
        std::fs::write(&replacement, b"replacement bytes\n").unwrap();
        let mut source = File::open(&path).unwrap();
        let stamp = SourceStamp::new(&source.metadata().unwrap()).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let owner = AgentCx::for_current_or_request();
            let deadline = Deadline::new();
            for _ in 0..2 {
                let mut command = helper_command(&owner, Path::new("/bin/sh"));
                command.args([
                    "-c",
                    "while IFS= read -r line; do printf '%s\\n' \"$line\"; done",
                ]);
                attach_source(&mut command, &mut source, &stamp).unwrap();
                let output = run_helper(command, &owner, &deadline, 64).await.unwrap();
                assert_eq!(output.stdout, b"original admitted bytes\n");
            }
        });
        assert_eq!(std::fs::read(path).unwrap(), b"replacement bytes\n");
    }

    #[cfg(unix)]
    #[test]
    fn subprocess_capture_enforces_combined_output_and_operation_deadline() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let owner = AgentCx::for_current_or_request();
            let mut command = helper_command(&owner, Path::new("/bin/sh"));
            command.args(["-c", "printf abc; printf def >&2"]);
            let error = capture(command, &owner, &Deadline::new(), 5)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("byte limit"));
            let mut command = helper_command(&owner, Path::new("/bin/sh"));
            command.args(["-c", "while :; do :; done"]);
            let deadline = Deadline(Instant::now() + Duration::from_millis(50));
            let started = Instant::now();
            let error = capture(command, &owner, &deadline, 0).await.unwrap_err();
            assert!(error.to_string().contains("VIDEO_TIMEOUT"));
            assert!(started.elapsed() < Duration::from_secs(5));
        });
    }

    #[test]
    fn a_read_beyond_the_concurrency_limit_waits_for_a_slot() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let owner = AgentCx::for_current_or_request();
            let deadline = Deadline::new();
            let mut held = Vec::new();
            for _ in 0..MAX_ACTIVE_READS {
                held.push(Permit::acquire(&owner, &deadline).await.unwrap());
            }
            let mut waiting = Box::pin(Permit::acquire(&owner, &deadline));
            assert!(futures::poll!(waiting.as_mut()).is_pending());
            drop(held.pop());
            waiting
                .await
                .expect("a released slot admits the waiting read");
        });
    }

    #[cfg(unix)]
    #[test]
    fn running_capture_observes_owner_cancellation() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let owner = AgentCx::for_current_or_request();
            let mut command = helper_command(&owner, Path::new("/bin/sh"));
            command.args(["-c", "while :; do :; done"]);
            let deadline = Deadline::new();
            let operation = capture(command, &owner, &deadline, 0);
            let cancel = async {
                owner.time().sleep(Duration::from_millis(20)).await;
                owner.cancel_with(
                    asupersync::types::CancelKind::User,
                    Some("video test cancellation"),
                );
            };
            let (result, ()) = futures::join!(operation, cancel);
            assert!(matches!(result, Err(Error::Aborted)));
        });
    }

    // An uncompressed two-frame AVI fixture, independent of an encoder binary.
    fn avi_fixture() -> Vec<u8> {
        fn chunk(kind: [u8; 4], payload: &[u8]) -> Vec<u8> {
            let mut bytes = kind.to_vec();
            bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            bytes.extend_from_slice(payload);
            if !payload.len().is_multiple_of(2) {
                bytes.push(0);
            }
            bytes
        }
        fn list(kind: [u8; 4], payload: &[u8]) -> Vec<u8> {
            let mut bytes = kind.to_vec();
            bytes.extend_from_slice(payload);
            chunk(*b"LIST", &bytes)
        }
        let mut avih = Vec::new();
        for value in [1_000_000_u32, 16, 0, 0, 2, 0, 1, 16, 2, 2, 0, 0, 0, 0] {
            avih.extend_from_slice(&value.to_le_bytes());
        }
        let mut strh = b"vidsDIB ".to_vec();
        for value in [0_u32, 0, 0, 1, 1, 0, 2, 16, u32::MAX, 0] {
            strh.extend_from_slice(&value.to_le_bytes());
        }
        for value in [0_u16, 0, 2, 2] {
            strh.extend_from_slice(&value.to_le_bytes());
        }
        let mut bitmap = Vec::new();
        for value in [40_u32, 2, 2] {
            bitmap.extend_from_slice(&value.to_le_bytes());
        }
        bitmap.extend_from_slice(&1_u16.to_le_bytes());
        bitmap.extend_from_slice(&24_u16.to_le_bytes());
        for value in [0_u32, 16, 0, 0, 0, 0] {
            bitmap.extend_from_slice(&value.to_le_bytes());
        }
        let mut stream = chunk(*b"strh", &strh);
        stream.extend(chunk(*b"strf", &bitmap));
        let mut header = chunk(*b"avih", &avih);
        header.extend(list(*b"strl", &stream));
        let mut movie = Vec::new();
        for bgr in [[0_u8, 0, 255], [0, 255, 0]] {
            let mut frame = Vec::new();
            for _ in 0..2 {
                frame.extend_from_slice(&bgr);
                frame.extend_from_slice(&bgr);
                frame.extend_from_slice(&[0, 0]); // Four-byte scanline alignment.
            }
            movie.extend(chunk(*b"00db", &frame));
        }
        let mut payload = b"AVI ".to_vec();
        payload.extend(list(*b"hdrl", &header));
        payload.extend(list(*b"movi", &movie));
        chunk(*b"RIFF", &payload)
    }

    #[test]
    #[ignore = "requires ffmpeg and ffprobe with the fd input protocol"]
    fn actual_decoder_reads_exact_frame_and_contact_sheet_from_descriptor() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("two-frames.avi");
        std::fs::write(&path, avi_fixture()).unwrap();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let owner = AgentCx::for_current_or_request();
            for selector in [
                Some(VideoSelector::Frame(1)),
                Some(VideoSelector::Time(1.0)),
                None,
            ] {
                let output = render_open_file(
                    File::open(&path).unwrap(),
                    path.clone(),
                    selector.clone(),
                    owner.clone(),
                )
                .await
                .unwrap();
                let ContentBlock::Image(image) = &output.content[1] else {
                    panic!("expected a normal provider-compatible image block");
                };
                let png = base64::engine::general_purpose::STANDARD
                    .decode(&image.data)
                    .unwrap();
                let (width, height, rgb) = decode_our_png(&png);
                if selector.is_some() {
                    let middle = (height as usize / 2 * width as usize + width as usize / 2) * 3;
                    assert_eq!(&rgb[middle..middle + 3], &[0, 255, 0]);
                } else {
                    assert_eq!(
                        output.details.as_ref().unwrap()["video"]["renderedFrames"],
                        6
                    );
                }
            }
        });
    }
}

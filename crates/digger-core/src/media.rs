//! Recordings and videos, through ffmpeg: 30-second windows of sound for the model, and a frame every few seconds,
//! one per shot.

use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::extract::ExtractError;
use crate::images;

/// A stretch of audio written to a WAV for the model, or a video frame written to a JPEG.
#[derive(Debug, Clone, PartialEq)]
pub struct Piece {
    pub path: String,
    pub start: f64,
    pub end: f64,
}

pub const SAMPLE_RATE: usize = 16_000;

fn unreadable(error: impl std::fmt::Display) -> ExtractError {
    ExtractError::Unreadable(error.to_string())
}

/// What a file holds: its length, and whether it has sound and pictures.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Probe {
    pub duration: f64,
    pub audio: bool,
    pub video: bool,
}

pub fn probe(path: &Path) -> Result<Probe, ExtractError> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration:stream=codec_type:stream_disposition=attached_pic",
            "-of",
            "default=noprint_wrappers=1",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .map_err(|_| ExtractError::Unreadable("ffmpeg isn't installed".into()))?;
    if !output.status.success() {
        return Err(unreadable(
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .next()
                .unwrap_or("ffprobe failed"),
        ));
    }
    let mut probe = Probe::default();
    let mut kind = "";
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        match line.split_once('=') {
            Some(("duration", value)) => probe.duration = value.parse().unwrap_or(0.0),
            Some(("codec_type", value)) => {
                kind = if value == "audio" {
                    "audio"
                } else if value == "video" {
                    "video"
                } else {
                    ""
                };
                if kind == "audio" {
                    probe.audio = true;
                }
                if kind == "video" {
                    probe.video = true;
                }
            }
            // A song's cover art is a video stream of one picture: not a video.
            Some(("DISPOSITION:attached_pic", "1")) if kind == "video" => probe.video = false,
            _ => {}
        }
    }
    Ok(probe)
}

/// Decodes audio (a file, or a video's soundtrack) to 16 kHz mono and writes `window`-second WAV pieces `hop` seconds
/// apart, skipping silent ones. One model input ignores anything past 30 s, so windows stay ≤ 30 s. Streams: memory
/// stays at one window, even for a 3-hour podcast. Returns the pieces and the duration.
pub fn audio_windows(path: &Path, window: f64, hop: f64, folder: &Path) -> Result<(Vec<Piece>, f64), ExtractError> {
    let minimum = 2.0;
    let probe = probe(path)?;
    if !probe.audio {
        return Ok((Vec::new(), probe.duration));
    }
    let mut child = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args(["-vn", "-ac", "1", "-ar", &SAMPLE_RATE.to_string(), "-f", "s16le", "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| ExtractError::Unreadable("ffmpeg isn't installed".into()))?;
    let mut stdout = child.stdout.take().unwrap();
    let (window_samples, hop_samples) = (
        (window * SAMPLE_RATE as f64) as usize,
        (hop * SAMPLE_RATE as f64) as usize,
    );
    let mut buffer: Vec<i16> = Vec::new();
    let mut offset = 0usize; // sample index of buffer[0] in the file
    let mut pieces = Vec::new();
    let emit = |samples: &[i16], offset: usize, pieces: &mut Vec<Piece>| -> Result<(), ExtractError> {
        if rms(samples) <= 0.003 {
            return Ok(()); // ≈ −50 dBFS: silence isn't worth a vector
        }
        let out = folder.join(format!("audio-{offset}.wav"));
        write_wav(samples, &out)?;
        pieces.push(Piece {
            path: out.to_string_lossy().into_owned(),
            start: offset as f64 / SAMPLE_RATE as f64,
            end: (offset + samples.len()) as f64 / SAMPLE_RATE as f64,
        });
        Ok(())
    };
    let mut bytes = vec![0u8; 1 << 16];
    let mut odd: Option<u8> = None;
    loop {
        let read = stdout.read(&mut bytes).map_err(unreadable)?;
        if read == 0 {
            break;
        }
        let mut data = &bytes[..read];
        if let Some(first) = odd.take() {
            buffer.push(i16::from_le_bytes([first, data[0]]));
            data = &data[1..];
        }
        let (pairs, rest) = data.as_chunks::<2>();
        buffer.extend(pairs.iter().map(|pair| i16::from_le_bytes(*pair)));
        odd = rest.first().copied();
        while buffer.len() >= window_samples {
            emit(&buffer[..window_samples], offset, &mut pieces)?;
            buffer.drain(..hop_samples);
            offset += hop_samples;
        }
    }
    let status = child.wait().map_err(unreadable)?;
    if !status.success() {
        let mut error = String::new();
        let _ = child.stderr.take().map(|mut stderr| stderr.read_to_string(&mut error));
        return Err(unreadable(
            error.lines().next().unwrap_or("audio decoding failed").to_string(),
        ));
    }
    // The tail: new audio beyond the previous window's overlap, or the whole file if it was shorter than a window.
    let already_covered = if offset == 0 { 0 } else { window_samples - hop_samples };
    if buffer.len().saturating_sub(already_covered) >= (minimum * SAMPLE_RATE as f64) as usize {
        emit(&buffer, offset, &mut pieces)?;
    }
    let duration = if probe.duration > 0.0 {
        probe.duration
    } else {
        (offset + buffer.len()) as f64 / SAMPLE_RATE as f64
    };
    Ok((pieces, duration))
}

fn rms(samples: &[i16]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|sample| (*sample as f64 / 32768.0).powi(2)).sum();
    (sum / samples.len() as f64).sqrt()
}

pub fn write_wav(samples: &[i16], out: &Path) -> Result<(), ExtractError> {
    let rate = SAMPLE_RATE as u32;
    let mut data = Vec::with_capacity(44 + samples.len() * 2);
    data.extend_from_slice(b"RIFF");
    data.extend_from_slice(&(36 + samples.len() as u32 * 2).to_le_bytes());
    data.extend_from_slice(b"WAVEfmt ");
    data.extend_from_slice(&16u32.to_le_bytes());
    data.extend_from_slice(&1u16.to_le_bytes()); // PCM
    data.extend_from_slice(&1u16.to_le_bytes()); // mono
    data.extend_from_slice(&rate.to_le_bytes());
    data.extend_from_slice(&(rate * 2).to_le_bytes());
    data.extend_from_slice(&2u16.to_le_bytes());
    data.extend_from_slice(&16u16.to_le_bytes());
    data.extend_from_slice(b"data");
    data.extend_from_slice(&(samples.len() as u32 * 2).to_le_bytes());
    for sample in samples {
        data.extend_from_slice(&sample.to_le_bytes());
    }
    fs::write(out, data).map_err(unreadable)
}

/// Samples a frame every `interval` seconds (more apart in a very long video: at most `max_frames`) and keeps one per
/// shot: a frame that looks like the previous kept one (8×8 average hash within 4 bits) only extends it. Screen
/// recordings and lectures shrink a lot. Returns the frames, the duration, and whether the video has sound.
pub fn keyframes(
    path: &Path,
    interval: f64,
    max_pixel: u32,
    max_frames: usize,
    folder: &Path,
) -> Result<(Vec<Piece>, f64, bool), ExtractError> {
    let probe = probe(path)?;
    if !probe.video || probe.duration <= 0.0 {
        return Ok((Vec::new(), probe.duration.max(0.0), probe.audio));
    }
    let step = interval.max(probe.duration / max_frames as f64);
    let first = 0.5f64.min(probe.duration / 2.0);
    let frames_folder = folder.join("frames");
    fs::create_dir_all(&frames_folder).map_err(unreadable)?;
    // One pass: ffmpeg turns the video upright, takes a frame every `step` from `first`, and scales it down.
    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-ss", &format!("{first:.3}"), "-i"])
        .arg(path)
        .args([
            "-an",
            "-vf",
            &format!(
                "fps=1/{step},scale='min({max_pixel},iw)':'min({max_pixel},ih)':force_original_aspect_ratio=decrease"
            ),
            "-q:v",
            "3",
        ])
        .arg(frames_folder.join("%06d.jpg"))
        .stdin(Stdio::null())
        .output()
        .map_err(|_| ExtractError::Unreadable("ffmpeg isn't installed".into()))?;
    if !output.status.success() {
        return Err(unreadable(
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .next()
                .unwrap_or("frame extraction failed")
                .to_string(),
        ));
    }
    let mut files: Vec<_> = fs::read_dir(&frames_folder)
        .map_err(unreadable)?
        .flatten()
        .map(|entry| entry.path())
        .collect();
    files.sort();
    let mut frames: Vec<Piece> = Vec::new();
    let mut last_hash: Option<u64> = None;
    for (index, file) in files.into_iter().enumerate() {
        let time = first + index as f64 * step;
        if index >= max_frames || time >= probe.duration {
            let _ = fs::remove_file(&file);
            continue;
        }
        let Ok(image) = image::open(&file) else { continue };
        let hash = images::average_hash(&image);
        if last_hash.is_some_and(|last| (hash ^ last).count_ones() <= 4) {
            let _ = fs::remove_file(&file);
            continue;
        }
        last_hash = Some(hash);
        frames.push(Piece {
            path: file.to_string_lossy().into_owned(),
            start: time,
            end: probe.duration,
        });
    }
    // Each kept frame stands for the stretch until the next kept frame.
    for index in 1..frames.len() {
        frames[index - 1].end = frames[index].start;
    }
    Ok((frames, probe.duration, probe.audio))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn have_ffmpeg() -> bool {
        Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_ok_and(|output| output.status.success())
    }

    #[test]
    fn audio_windows_overlap_and_skip_silence() {
        if !have_ffmpeg() {
            eprintln!("skipped: no ffmpeg");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // 70 s: a tone for 40 s, then 30 s of silence.
        let samples: Vec<i16> = (0..70 * SAMPLE_RATE)
            .map(|i| {
                if i < 40 * SAMPLE_RATE {
                    ((i as f64 * 440.0 * std::f64::consts::TAU / SAMPLE_RATE as f64).sin() * 8000.0) as i16
                } else {
                    0
                }
            })
            .collect();
        let wav = dir.path().join("talk.wav");
        write_wav(&samples, &wav).unwrap();
        let (pieces, duration) = audio_windows(&wav, 30.0, 25.0, dir.path()).unwrap();
        assert!((duration - 70.0).abs() < 0.1);
        // Windows at 0, 25 and 50 s; the one at 50 s is silent, and the 45 s tail beyond it too.
        let starts: Vec<f64> = pieces.iter().map(|piece| piece.start).collect();
        assert_eq!(starts, [0.0, 25.0]);
        assert_eq!(pieces[0].end, 30.0);
    }

    #[test]
    fn keyframes_keep_one_frame_per_shot() {
        if !have_ffmpeg() {
            eprintln!("skipped: no ffmpeg");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let video = dir.path().join("clip.mp4");
        // 12 s: 6 s of one still picture, then 6 s of another, with a tone.
        let status = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=navy:s=640x360:d=6",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=640x360:d=6:r=25",
                "-f",
                "lavfi",
                "-i",
                "sine=f=440:d=12",
                "-filter_complex",
                "[0:v]fps=25[a];[a][1:v]concat=n=2:v=1[v]",
                "-map",
                "[v]",
                "-map",
                "2:a",
                "-pix_fmt",
                "yuv420p",
                "-shortest",
            ])
            .arg(&video)
            .status()
            .unwrap();
        assert!(status.success());
        let (frames, duration, audio) = keyframes(&video, 3.0, 1024, 1200, dir.path()).unwrap();
        assert!(audio);
        assert!((duration - 12.0).abs() < 0.2, "{duration}");
        assert_eq!(frames[0].start, 0.5);
        assert!(frames.len() >= 2 && frames.len() < 4, "{frames:?}"); // the still shot is one frame
        assert_eq!(frames[0].end, frames[1].start);
        assert_eq!(image::image_dimensions(&frames[0].path).unwrap(), (640, 360));
    }
}

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

pub const PROBE_DEADLINE: Duration = Duration::from_secs(5);
pub const CONVERT_DEADLINE: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CodecPlan {
    pub plan_id: String,
    pub ops: Vec<CodecOp>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CodecOp {
    pub input_path: PathBuf,
    pub output_path: PathBuf,
    pub spec_id: String,
    pub format: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub long_edge: Option<u32>,
    pub fit: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CodecOutput {
    pub spec_id: String,
    pub width: u32,
    pub height: u32,
    pub sha256: String,
    pub size: u64,
    pub mime: String,
    pub output_path: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProbeResult {
    pub mime: String,
    pub width: u32,
    pub height: u32,
    pub frames: u32,
    pub orientation: u8,
}

pub fn browser_inline_safe(mime: &str) -> bool {
    matches!(
        mime,
        "image/jpeg" | "image/png" | "image/gif" | "image/webp" | "image/bmp"
    )
}

const STATIC_PIXELS: u64 = 64_000_000;
const ANIMATION_PIXELS: u64 = 120_000_000;
const CODEC_VERSION: &str = "image-0.25";

pub fn probe_bytes(bytes: &[u8]) -> Result<ProbeResult> {
    let mime = crate::storage::detect_mime(bytes).ok_or_else(|| anyhow!("unsupported image"))?;
    if mime == "image/svg+xml" {
        bail!("svg not allowed");
    }
    let (w, h, frames, pixels) = if mime == "image/gif" {
        gif_frame_stats(bytes)?
    } else if mime == "image/tiff" {
        tiff_page_stats(bytes)?
    } else {
        let img = image::load_from_memory(bytes)?;
        let (w, h) = img.dimensions_u32();
        (w, h, 1, w as u64 * h as u64)
    };
    if w.max(h) > 16384 {
        bail!("edge too large");
    }
    if !frame_budget_ok(pixels, frames) {
        bail!("pixel budget exceeded");
    }
    Ok(ProbeResult {
        mime: mime.to_string(),
        width: w,
        height: h,
        frames,
        orientation: jpeg_orientation(bytes),
    })
}

fn frame_budget_ok(pixels: u64, frames: u32) -> bool {
    if frames <= 1 {
        pixels <= STATIC_PIXELS
    } else {
        pixels <= ANIMATION_PIXELS
    }
}

fn gif_frame_stats(bytes: &[u8]) -> Result<(u32, u32, u32, u64)> {
    use image::AnimationDecoder;
    let decoder = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes))
        .map_err(|err| anyhow!("permanent:UNDECODABLE {err}"))?;
    let mut frames = 0u32;
    let mut pixels = 0u64;
    let mut width = 0u32;
    let mut height = 0u32;
    for frame in decoder.into_frames() {
        let frame = frame.map_err(|err| anyhow!("permanent:UNDECODABLE {err}"))?;
        let (w, h) = (frame.buffer().width(), frame.buffer().height());
        if frames == 0 {
            width = w;
            height = h;
        }
        frames += 1;
        pixels = pixels.saturating_add(w as u64 * h as u64);
        if frames > 10_000 || !frame_budget_ok(pixels, frames.max(2)) {
            bail!("pixel budget exceeded");
        }
    }
    if frames == 0 {
        bail!("permanent:UNDECODABLE empty gif");
    }
    Ok((width, height, frames, pixels))
}

fn tiff_page_stats(bytes: &[u8]) -> Result<(u32, u32, u32, u64)> {
    let mut decoder = tiff::decoder::Decoder::new(std::io::Cursor::new(bytes))
        .map_err(|err| anyhow!("permanent:UNDECODABLE {err}"))?;
    let (width, height) = decoder
        .dimensions()
        .map_err(|err| anyhow!("permanent:UNDECODABLE {err}"))?;
    let mut frames = 1u32;
    let mut pixels = width as u64 * height as u64;
    while decoder.more_images() {
        decoder
            .next_image()
            .map_err(|err| anyhow!("permanent:UNDECODABLE {err}"))?;
        let (page_w, page_h) = decoder
            .dimensions()
            .map_err(|err| anyhow!("permanent:UNDECODABLE {err}"))?;
        frames += 1;
        pixels = pixels.saturating_add(page_w as u64 * page_h as u64);
        if frames > 10_000 || !frame_budget_ok(pixels, frames.max(2)) {
            bail!("pixel budget exceeded");
        }
    }
    if width == 0 || height == 0 {
        bail!("permanent:UNDECODABLE empty tiff");
    }
    Ok((width, height, frames, pixels))
}

fn first_tiff_page(reader: impl BufRead + Seek) -> Result<image::DynamicImage> {
    let decoder = image::codecs::tiff::TiffDecoder::new(reader)
        .map_err(|err| anyhow!("permanent:UNDECODABLE {err}"))?;
    image::DynamicImage::from_decoder(decoder)
        .map_err(|err| anyhow!("permanent:UNDECODABLE {err}"))
}

fn first_gif_frame(reader: impl BufRead + Seek) -> Result<image::DynamicImage> {
    use image::AnimationDecoder;
    let decoder = image::codecs::gif::GifDecoder::new(reader)
        .map_err(|err| anyhow!("permanent:UNDECODABLE {err}"))?;
    let frame = decoder
        .into_frames()
        .next()
        .ok_or_else(|| anyhow!("permanent:UNDECODABLE empty gif"))?
        .map_err(|err| anyhow!("permanent:UNDECODABLE {err}"))?;
    Ok(image::DynamicImage::ImageRgba8(frame.into_buffer()))
}

pub fn jpeg_orientation(bytes: &[u8]) -> u8 {
    read_jpeg_orientation(&mut std::io::Cursor::new(bytes)).unwrap_or(1)
}

fn read_jpeg_orientation(reader: &mut (impl Read + Seek)) -> std::io::Result<u8> {
    let mut header = [0; 2];
    reader.read_exact(&mut header)?;
    if header != [0xff, 0xd8] {
        return Ok(1);
    }
    loop {
        reader.read_exact(&mut header)?;
        if header[0] != 0xff || matches!(header[1], 0xd9 | 0xda) {
            return Ok(1);
        }
        let marker = header[1];
        reader.read_exact(&mut header)?;
        let Some(length) = u16::from_be_bytes(header).checked_sub(2) else {
            return Ok(1);
        };
        if marker == 0xe1 {
            let mut segment = vec![0; usize::from(length)];
            reader.read_exact(&mut segment)?;
            if let Some(orientation) = exif_orientation(&segment) {
                return Ok(orientation);
            }
        } else {
            reader.seek(SeekFrom::Current(i64::from(length)))?;
        }
    }
}

pub fn decode_file(path: &Path) -> Result<image::DynamicImage> {
    let reader = image::ImageReader::new(BufReader::new(std::fs::File::open(path)?))
        .with_guessed_format()?;
    match reader.format() {
        Some(image::ImageFormat::Gif) => first_gif_frame(reader.into_inner()),
        Some(image::ImageFormat::Tiff) => first_tiff_page(reader.into_inner()),
        _ => reader.decode().map_err(|err| anyhow!("permanent:UNDECODABLE {err}")),
    }
}

fn exif_orientation(payload: &[u8]) -> Option<u8> {
    if payload.len() < 16 || &payload[..6] != b"Exif\0\0" {
        return None;
    }
    let tiff = &payload[6..];
    let little = match tiff.get(..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |offset: usize| -> Option<u16> {
        let bytes = tiff.get(offset..offset + 2)?;
        Some(if little {
            u16::from_le_bytes([bytes[0], bytes[1]])
        } else {
            u16::from_be_bytes([bytes[0], bytes[1]])
        })
    };
    let u32_at = |offset: usize| -> Option<u32> {
        let bytes = tiff.get(offset..offset + 4)?;
        Some(if little {
            u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
        } else {
            u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
        })
    };
    if u16_at(2)? != 42 {
        return None;
    }
    let ifd = u32_at(4)? as usize;
    let count = u16_at(ifd)? as usize;
    for entry in 0..count {
        let at = ifd + 2 + entry * 12;
        if u16_at(at)? == 0x0112 && u16_at(at + 2)? == 3 {
            let value = u16_at(at + 8)?;
            if (1..=8).contains(&value) {
                return Some(value as u8);
            }
        }
    }
    None
}

fn apply_orientation(img: image::DynamicImage, orientation: u8) -> image::DynamicImage {
    match orientation {
        2 => img.fliph(),
        3 => img.rotate180(),
        4 => img.flipv(),
        5 => img.rotate270().fliph(),
        6 => img.rotate90(),
        7 => img.rotate90().fliph(),
        8 => img.rotate270(),
        _ => img,
    }
}

trait DimExt {
    fn dimensions_u32(&self) -> (u32, u32);
}
impl DimExt for image::DynamicImage {
    fn dimensions_u32(&self) -> (u32, u32) {
        (self.width(), self.height())
    }
}

pub fn run_plan(plan: &CodecPlan) -> Result<Vec<CodecOutput>> {
    let mut out = Vec::new();
    for op in &plan.ops {
        let orientation = read_jpeg_orientation(&mut BufReader::new(std::fs::File::open(&op.input_path)?))
            .unwrap_or(1);
        let img = apply_orientation(decode_file(&op.input_path)?, orientation);
        let format = match op.format.as_str() {
            "jpeg" | "jpg" => image::ImageFormat::Jpeg,
            "webp" => image::ImageFormat::WebP,
            _ => image::ImageFormat::Png,
        };
        let transformed = transform(img, op)?;
        let transformed = if format == image::ImageFormat::Jpeg {
            flatten_on_white(transformed)
        } else {
            transformed
        };
        if let Some(parent) = op.output_path.parent() {
            std::fs::create_dir_all(parent)?;
        };
        transformed.save_with_format(&op.output_path, format)?;
        let (size, sha256) = crate::storage::local::hash_file(&op.output_path)?;
        if size == 0 {
            bail!("empty codec output");
        }
        let verify = decode_file(&op.output_path)?;
        out.push(CodecOutput {
            spec_id: op.spec_id.clone(),
            width: verify.width(),
            height: verify.height(),
            sha256,
            size,
            mime: match format {
                image::ImageFormat::Jpeg => "image/jpeg".into(),
                image::ImageFormat::WebP => "image/webp".into(),
                _ => "image/png".into(),
            },
            output_path: op.output_path.clone(),
        });
    }
    Ok(out)
}

fn transform(img: image::DynamicImage, op: &CodecOp) -> Result<image::DynamicImage> {
    let (sw, sh) = (img.width(), img.height());
    if op.fit == "cover" {
        let tw = op.width.unwrap_or(256).max(1);
        let th = op.height.unwrap_or(256).max(1);
        let scale = (tw as f32 / sw as f32).max(th as f32 / sh as f32).min(1.0);
        let scaled = if scale < 1.0 {
            let nw = ((sw as f32) * scale).round().max(1.0) as u32;
            let nh = ((sh as f32) * scale).round().max(1.0) as u32;
            img.resize_exact(nw, nh, image::imageops::FilterType::Lanczos3)
        } else {
            img
        };
        let cw = scaled.width().min(tw);
        let ch = scaled.height().min(th);
        let x = scaled.width().saturating_sub(cw) / 2;
        let y = scaled.height().saturating_sub(ch) / 2;
        Ok(scaled.crop_imm(x, y, cw, ch))
    } else {
        let long = op.long_edge.unwrap_or(op.width.unwrap_or(sw).max(op.height.unwrap_or(sh)));
        let current_long = sw.max(sh);
        if current_long <= long {
            return Ok(img);
        }
        let scale = long as f32 / current_long as f32;
        let nw = ((sw as f32) * scale).round().max(1.0) as u32;
        let nh = ((sh as f32) * scale).round().max(1.0) as u32;
        Ok(img.resize_exact(nw, nh, image::imageops::FilterType::Lanczos3))
    }
}

fn flatten_on_white(img: image::DynamicImage) -> image::DynamicImage {
    if !img.color().has_alpha() {
        return img;
    }
    let rgba = img.to_rgba8();
    let mut flat = image::RgbImage::new(rgba.width(), rgba.height());
    for (dest, src) in flat.pixels_mut().zip(rgba.pixels()) {
        let alpha = src[3] as f32 / 255.0;
        let mix = |channel: u8| (channel as f32 * alpha + 255.0 * (1.0 - alpha)).round() as u8;
        *dest = image::Rgb([mix(src[0]), mix(src[1]), mix(src[2])]);
    }
    image::DynamicImage::ImageRgb8(flat)
}

pub fn execute_plan(plan: &CodecPlan) -> Result<Vec<CodecOutput>> {
    let started = std::time::Instant::now();
    let format = plan
        .ops
        .first()
        .map(|op| op.format.as_str())
        .unwrap_or("none");
    match run_plan(plan) {
        Ok(outputs) => {
            tracing::info!("{}", codec_log_line(&plan.plan_id, format, started.elapsed(), "ok"));
            Ok(outputs)
        }
        Err(err) => {
            let code = codec_error_code(&err);
            let line = codec_log_line(&plan.plan_id, format, started.elapsed(), &code);
            tracing::info!("{line}");
            bail!("{line}");
        }
    }
}

fn codec_log_line(plan_id: &str, format: &str, elapsed: Duration, error_code: &str) -> String {
    format!(
        "codec plan_id={} codec={} format={} elapsed_ms={} error_code={}",
        safe_log_token(plan_id),
        CODEC_VERSION,
        safe_log_token(format),
        elapsed.as_millis(),
        safe_log_token(error_code)
    )
}

fn safe_log_token(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | ':' | '.') {
                ch
            } else {
                '_'
            }
        })
        .take(80)
        .collect()
}

fn codec_error_code(err: &anyhow::Error) -> String {
    let text = err.to_string();
    if let Some(rest) = text.strip_prefix("permanent:") {
        let token = rest
            .split(|ch: char| ch.is_whitespace() || ch == '/' || ch == '\\')
            .next()
            .unwrap_or("UNDECODABLE");
        if token
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == ':')
            && !token.is_empty()
        {
            return format!("permanent:{token}");
        }
    }
    if text.contains("pixel budget") {
        return "PIXEL_BUDGET".into();
    }
    if text.contains("edge too large") {
        return "EDGE_LIMIT".into();
    }
    if text.contains("empty codec") {
        return "EMPTY_OUTPUT".into();
    }
    "CODEC_FAILED".into()
}

pub async fn run_in_worker(
    bin: &Path,
    plan: &CodecPlan,
    deadline: Duration,
    attempt_dir: Option<&Path>,
) -> Result<Vec<CodecOutput>> {
    let mut command = tokio::process::Command::new(bin);
    command
        .arg("codec-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn()?;
    let pid = child.id().ok_or_else(|| anyhow!("codec worker pid"))?;
    let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("stdin"))?;
    let payload = serde_json::to_vec(plan)?;
    stdin.write_all(&payload).await?;
    stdin.write_all(b"\n").await?;
    drop(stdin);
    let collected = tokio::time::timeout(deadline, child.wait_with_output()).await;
    let out = match collected {
        Ok(out) => out?,
        Err(_) => {
            kill_process_group(pid);
            if let Some(dir) = attempt_dir {
                let _ = std::fs::remove_dir_all(dir);
            }
            bail!("codec deadline plan_id={}", plan.plan_id);
        }
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if let Some(line) = stderr
            .lines()
            .rev()
            .find(|line| line.starts_with("codec ") && !line.contains('/') && !line.contains('\\'))
        {
            bail!("{line}");
        }
        bail!("codec plan_id={} codec={CODEC_VERSION} format=unknown elapsed_ms=0 error_code=CODEC_FAILED", safe_log_token(&plan.plan_id));
    }
    let line = std::str::from_utf8(&out.stdout)?.lines().next().unwrap_or("[]");
    Ok(serde_json::from_str(line)?)
}

fn kill_process_group(pid: u32) {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
        let mut status = 0;
        libc::waitpid(pid as i32, &mut status, 0);
    }
}

pub fn codec_worker_main() -> Result<()> {
    let mut input = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)?;
    let plan: CodecPlan = serde_json::from_str(input.trim())?;
    let outputs = execute_plan(&plan).map_err(|err| {
        let line = err.to_string();
        eprintln!("{line}");
        err
    })?;
    println!("{}", serde_json::to_string(&outputs)?);
    Ok(())
}

pub fn default_specs() -> Vec<crate::versions::VariantSpec> {
    crate::versions::default_specs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cover(width: u32, height: u32) -> CodecOp {
        CodecOp {
            input_path: PathBuf::new(),
            output_path: PathBuf::new(),
            spec_id: "thumb".into(),
            format: "png".into(),
            width: Some(width),
            height: Some(height),
            long_edge: None,
            fit: "cover".into(),
        }
    }

    #[test]
    fn codec_failure_log_keeps_the_plan_and_drops_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("original.bin");
        let output = dir.path().join("thumb.png");
        let plan = CodecPlan {
            plan_id: "job/image:thumb".into(),
            ops: vec![CodecOp {
                input_path: missing,
                output_path: output,
                spec_id: "thumb".into(),
                format: "png".into(),
                width: Some(32),
                height: Some(32),
                long_edge: None,
                fit: "cover".into(),
            }],
        };
        let message = execute_plan(&plan).unwrap_err().to_string();
        assert!(message.starts_with("codec "));
        assert!(message.contains("plan_id=job_image:thumb"));
        assert!(message.contains("codec=image-0.25"));
        assert!(message.contains("format=png"));
        assert!(message.contains("elapsed_ms="));
        assert!(message.contains("error_code=CODEC_FAILED"));
        assert!(!message.contains(dir.path().to_string_lossy().as_ref()));
        assert!(!message.contains("original.bin"));
        assert!(!message.contains("thumb.png"));
    }

    #[test]
    fn gif_preview_uses_the_first_frame_only() {
        assert!(frame_budget_ok(64_000_000, 1));
        assert!(!frame_budget_ok(64_000_001, 1));
        assert!(frame_budget_ok(120_000_000, 3));
        assert!(!frame_budget_ok(120_000_001, 3));
        let mut encoded = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut encoded);
            encoder
                .encode_frame(image::Frame::new(image::RgbaImage::from_pixel(
                    2,
                    2,
                    image::Rgba([255, 0, 0, 255]),
                )))
                .unwrap();
            encoder
                .encode_frame(image::Frame::new(image::RgbaImage::from_pixel(
                    2,
                    2,
                    image::Rgba([0, 0, 255, 255]),
                )))
                .unwrap();
        }
        let probe = probe_bytes(&encoded).unwrap();
        assert_eq!(probe.frames, 2);
        assert_eq!((probe.width, probe.height), (2, 2));
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in.gif");
        let output = dir.path().join("out.png");
        std::fs::write(&input, &encoded).unwrap();
        let plan = CodecPlan {
            plan_id: "gif".into(),
            ops: vec![CodecOp {
                input_path: input,
                output_path: output.clone(),
                spec_id: "thumb".into(),
                format: "png".into(),
                width: None,
                height: None,
                long_edge: Some(64),
                fit: "inside".into(),
            }],
        };
        run_plan(&plan).unwrap();
        let png = image::open(&output).unwrap().to_rgba8();
        assert_eq!(png.get_pixel(0, 0).0, [255, 0, 0, 255]);
    }

    #[test]
    fn tiff_preview_uses_the_first_page_only() {
        let mut encoded = std::io::Cursor::new(Vec::new());
        {
            let mut encoder = tiff::encoder::TiffEncoder::new(&mut encoded).unwrap();
            encoder
                .write_image::<tiff::encoder::colortype::RGB8>(2, 2, &[255, 0, 0, 255, 0, 0, 255, 0, 0, 255, 0, 0])
                .unwrap();
            encoder
                .write_image::<tiff::encoder::colortype::RGB8>(4, 1, &[0, 0, 255, 0, 0, 255, 0, 0, 255, 0, 0, 255])
                .unwrap();
        }
        let bytes = encoded.into_inner();
        let probe = probe_bytes(&bytes).unwrap();
        assert_eq!(probe.mime, "image/tiff");
        assert_eq!(probe.frames, 2);
        assert_eq!((probe.width, probe.height), (2, 2));
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in.tiff");
        let output = dir.path().join("out.png");
        std::fs::write(&input, &bytes).unwrap();
        let plan = CodecPlan {
            plan_id: "tiff".into(),
            ops: vec![CodecOp {
                input_path: input,
                output_path: output.clone(),
                spec_id: "thumb".into(),
                format: "png".into(),
                width: None,
                height: None,
                long_edge: Some(64),
                fit: "inside".into(),
            }],
        };
        run_plan(&plan).unwrap();
        let png = image::open(&output).unwrap().to_rgb8();
        assert_eq!(png.dimensions(), (2, 2));
        assert_eq!(png.get_pixel(0, 0).0, [255, 0, 0]);
    }

    #[tokio::test]
    async fn deadline_kills_the_worker_group_and_clears_the_attempt() {
        assert!(PROBE_DEADLINE < CONVERT_DEADLINE);
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("sleeper.sh");
        std::fs::write(&bin, "#!/bin/sh\nsleep 30\n").unwrap();
        let mut permissions = std::fs::metadata(&bin).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o755);
        std::fs::set_permissions(&bin, permissions).unwrap();
        let attempt = dir.path().join("attempt");
        std::fs::create_dir(&attempt).unwrap();
        std::fs::write(attempt.join("original.bin"), b"pixels").unwrap();
        let plan = CodecPlan {
            plan_id: "slow".into(),
            ops: vec![],
        };
        let err = run_in_worker(&bin, &plan, Duration::from_millis(200), Some(&attempt))
            .await
            .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("codec deadline"));
        assert!(message.contains("plan_id=slow"));
        assert!(!message.contains(attempt.to_string_lossy().as_ref()));
        assert!(!attempt.exists());
    }

    #[test]
    fn cover_does_not_enlarge_and_keeps_the_center() {
        let small = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(8, 8, image::Rgb([9, 8, 7])));
        let kept = transform(small, &cover(256, 256)).unwrap();
        assert_eq!((kept.width(), kept.height()), (8, 8));

        let wide = image::RgbImage::from_fn(40, 10, |x, _| {
            if x < 20 {
                image::Rgb([0, 0, 0])
            } else {
                image::Rgb([255, 0, 0])
            }
        });
        let cropped = transform(image::DynamicImage::ImageRgb8(wide), &cover(10, 10)).unwrap();
        assert_eq!((cropped.width(), cropped.height()), (10, 10));
        let rgb = cropped.to_rgb8();
        assert_eq!(rgb.get_pixel(0, 5).0, [0, 0, 0]);
        assert_eq!(rgb.get_pixel(9, 5).0, [255, 0, 0]);
    }

    #[test]
    fn jpeg_flattens_transparency_onto_white() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in.png");
        let output = dir.path().join("out.jpg");
        let mut image = image::RgbaImage::new(64, 16);
        for y in 0..16 {
            for x in 0..32 {
                image.put_pixel(x, y, image::Rgba([255, 0, 0, 0]));
            }
            for x in 32..64 {
                image.put_pixel(x, y, image::Rgba([0, 0, 255, 255]));
            }
        }
        image.save(&input).unwrap();
        let plan = CodecPlan {
            plan_id: "jpeg".into(),
            ops: vec![CodecOp {
                input_path: input,
                output_path: output.clone(),
                spec_id: "flat".into(),
                format: "jpeg".into(),
                width: None,
                height: None,
                long_edge: Some(64),
                fit: "inside".into(),
            }],
        };
        let out = run_plan(&plan).unwrap();
        assert_eq!(out[0].mime, "image/jpeg");
        let decoded = image::open(&output).unwrap().to_rgb8();
        let white = decoded.get_pixel(4, 8).0;
        assert!(white.iter().all(|channel| *channel >= 250), "{white:?}");
        let blue = decoded.get_pixel(60, 8).0;
        assert!(blue[2] > 200 && blue[0] < 40, "{blue:?}");
    }
}

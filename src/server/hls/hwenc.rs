//! H.264 hardware-encoder detection for the HLS transcode path.
//!
//! Picked once at server startup, stored on `AppState`, plumbed into each
//! `ProducerCtx`. The producer rewrites its ffmpeg argv per backend; if a
//! hwenc spawn fails on first launch, a process-wide sticky flag downgrades
//! every subsequent producer to libx264 (see `producer.rs`).
//!
//! Auto-detect rules:
//! * `BINKFLIX_HWACCEL` (env): `auto` (default), `none`, `vaapi`, `qsv`,
//!   `videotoolbox`. Explicit values are validated against `ffmpeg -encoders`
//!   and downgraded to `None` (with a warning) if the encoder isn't
//!   compiled into the local ffmpeg.
//! * `auto`:
//!   - Linux: prefer VAAPI if `/dev/dri/renderD*` exists and `h264_vaapi`
//!     is listed; else QSV under the same gate.
//!   - macOS: prefer VideoToolbox if `h264_videotoolbox` is listed.
//!   - Otherwise None.

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HwEncoder {
    None,
    Vaapi,
    Qsv,
    VideoToolbox,
}

impl HwEncoder {
    /// ffmpeg encoder name; `libx264` for `None` so callers don't need to
    /// special-case software in their argv builder.
    pub fn ffmpeg_name(self) -> &'static str {
        match self {
            HwEncoder::None => "libx264",
            HwEncoder::Vaapi => "h264_vaapi",
            HwEncoder::Qsv => "h264_qsv",
            HwEncoder::VideoToolbox => "h264_videotoolbox",
        }
    }
}

pub async fn detect() -> HwEncoder {
    let requested = std::env::var("BINKFLIX_HWACCEL")
        .ok()
        .map(|s| s.trim().to_ascii_lowercase())
        .unwrap_or_else(|| "auto".to_string());

    if requested == "none" {
        tracing::info!("hwenc: none (BINKFLIX_HWACCEL=none)");
        return HwEncoder::None;
    }

    let listed = match list_h264_encoders().await {
        Ok(set) => set,
        Err(e) => {
            tracing::warn!(error = %e, "hwenc: ffmpeg -encoders probe failed; defaulting to software");
            return HwEncoder::None;
        }
    };

    let pick = match requested.as_str() {
        "auto" => auto_pick(&listed),
        "vaapi" => validate_explicit(HwEncoder::Vaapi, &listed),
        "qsv" => validate_explicit(HwEncoder::Qsv, &listed),
        "videotoolbox" => validate_explicit(HwEncoder::VideoToolbox, &listed),
        other => {
            tracing::warn!(value = other, "hwenc: unknown BINKFLIX_HWACCEL; defaulting to software");
            HwEncoder::None
        }
    };

    match pick {
        HwEncoder::None => tracing::info!("hwenc: none (no h264 hw encoder available)"),
        other => tracing::info!(encoder = other.ffmpeg_name(), "hwenc: detected"),
    }
    pick
}

fn auto_pick(listed: &EncoderSet) -> HwEncoder {
    #[cfg(target_os = "linux")]
    {
        if has_dri_render_device() {
            if listed.vaapi {
                return HwEncoder::Vaapi;
            }
            if listed.qsv {
                return HwEncoder::Qsv;
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        if listed.videotoolbox {
            return HwEncoder::VideoToolbox;
        }
    }
    let _ = listed;
    HwEncoder::None
}

fn validate_explicit(want: HwEncoder, listed: &EncoderSet) -> HwEncoder {
    let ok = match want {
        HwEncoder::Vaapi => listed.vaapi,
        HwEncoder::Qsv => listed.qsv,
        HwEncoder::VideoToolbox => listed.videotoolbox,
        HwEncoder::None => true,
    };
    if !ok {
        tracing::warn!(
            encoder = want.ffmpeg_name(),
            "hwenc: requested encoder not present in ffmpeg build; falling back to software"
        );
        return HwEncoder::None;
    }
    #[cfg(target_os = "linux")]
    {
        if matches!(want, HwEncoder::Vaapi | HwEncoder::Qsv) && !has_dri_render_device() {
            tracing::warn!(
                encoder = want.ffmpeg_name(),
                "hwenc: no /dev/dri/renderD* present; falling back to software"
            );
            return HwEncoder::None;
        }
    }
    want
}

struct EncoderSet {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    vaapi: bool,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    qsv: bool,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    videotoolbox: bool,
}

async fn list_h264_encoders() -> std::io::Result<EncoderSet> {
    let out = tokio::process::Command::new("ffmpeg")
        .arg("-hide_banner")
        .arg("-loglevel").arg("error")
        .arg("-encoders")
        .output()
        .await?;
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(EncoderSet {
        vaapi: text.contains("h264_vaapi"),
        qsv: text.contains("h264_qsv"),
        videotoolbox: text.contains("h264_videotoolbox"),
    })
}

#[cfg(target_os = "linux")]
fn has_dri_render_device() -> bool {
    let Ok(read) = std::fs::read_dir("/dev/dri") else {
        return false;
    };
    for entry in read.flatten() {
        if let Some(name) = entry.file_name().to_str() {
            if name.starts_with("renderD") {
                return true;
            }
        }
    }
    false
}

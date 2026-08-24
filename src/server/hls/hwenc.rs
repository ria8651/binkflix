//! H.264 hardware-encoder detection for the HLS transcode path.
//!
//! Picked once at server startup, stored on `AppState`, plumbed into each
//! `ProducerCtx`. The producer rewrites its ffmpeg argv per backend, and
//! falls back to libx264 per *launch* if a hwenc ffmpeg dies during startup
//! (see `producer.rs`) — nothing is remembered process-wide, so a transient
//! device failure can't outlive the stream that hit it.
//!
//! Auto-detect rules:
//! * `BINKFLIX_HWACCEL` (env): `auto` (default), `none`, `vaapi`, `qsv`,
//!   `videotoolbox`.
//! * `auto`:
//!   - Linux: prefer VAAPI if `/dev/dri/renderD*` exists and `h264_vaapi`
//!     is listed; else QSV under the same gate.
//!   - macOS: prefer VideoToolbox if `h264_videotoolbox` is listed.
//!   - Otherwise None.
//!
//! Naming a hardware encoder explicitly is a declaration of intent, so it
//! also selects **strict** mode: libx264 can't hold real-time on the
//! hardware this runs on, which makes a silent software substitution an
//! outage that merely *looks* like a slow server. Under strict mode a hwenc
//! failure surfaces as an error on the transcode path instead — at startup
//! (`unmet`: encoder or device missing) and at runtime (a hwenc ffmpeg
//! dying during launch). `auto`/unset keeps best-effort fallback; `none` is
//! software by choice.
//!
//! Strict failures deliberately don't stop the server: direct play and
//! remux never touch the GPU, so only transcodes fail.

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

    /// `BINKFLIX_HWACCEL` value that selects this backend — so an error
    /// message can name the knob the operator actually set.
    pub fn env_value(self) -> &'static str {
        match self {
            HwEncoder::None => "none",
            HwEncoder::Vaapi => "vaapi",
            HwEncoder::Qsv => "qsv",
            HwEncoder::VideoToolbox => "videotoolbox",
        }
    }
}

/// Resolved hardware-encoder configuration, pinned for the process.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct HwEncConfig {
    /// Encoder producers are launched with. `None` means libx264.
    pub encoder: HwEncoder,
    /// An explicit hardware encoder was requested: never substitute
    /// software for it, fail the transcode instead.
    pub strict: bool,
    /// Set when a strict request couldn't be satisfied at startup. Transcode
    /// requests fail with this reason; `encoder` is left `None` so nothing
    /// spawns a doomed ffmpeg. Direct/remux are unaffected.
    pub unmet: Option<&'static str>,
}

impl HwEncConfig {
    fn lenient(encoder: HwEncoder) -> Self {
        Self { encoder, strict: false, unmet: None }
    }

    fn strict(encoder: HwEncoder) -> Self {
        Self { encoder, strict: true, unmet: None }
    }

    fn unmet(reason: &'static str) -> Self {
        Self { encoder: HwEncoder::None, strict: true, unmet: Some(reason) }
    }

    /// Encoder name to report for a transcode. Per-launch fallback isn't
    /// visible here — this is what producers are *asked* to use.
    pub fn requested_name(&self) -> &'static str {
        self.encoder.ffmpeg_name()
    }
}

pub async fn detect() -> HwEncConfig {
    let requested = std::env::var("BINKFLIX_HWACCEL")
        .ok()
        .map(|s| s.trim().to_ascii_lowercase())
        .unwrap_or_else(|| "auto".to_string());

    if requested == "none" {
        tracing::info!("hwenc: none (BINKFLIX_HWACCEL=none)");
        return HwEncConfig::lenient(HwEncoder::None);
    }

    // A named hardware encoder is a promise the operator wants kept; `auto`
    // (and unset) is a request to do the best we can.
    let want = match requested.as_str() {
        "auto" => None,
        "vaapi" => Some(HwEncoder::Vaapi),
        "qsv" => Some(HwEncoder::Qsv),
        "videotoolbox" => Some(HwEncoder::VideoToolbox),
        other => {
            // A typo'd encoder name used to fall through to software
            // silently — precisely the failure mode strict mode exists to
            // stop. Treat it as an unsatisfiable explicit request.
            tracing::error!(
                value = other,
                "hwenc: unknown BINKFLIX_HWACCEL; transcodes will fail until it is fixed"
            );
            return HwEncConfig::unmet("unknown BINKFLIX_HWACCEL value");
        }
    };

    let listed = match list_h264_encoders().await {
        Ok(set) => set,
        Err(e) => {
            return match want {
                Some(enc) => {
                    tracing::error!(error = %e, encoder = enc.ffmpeg_name(),
                        "hwenc: ffmpeg -encoders probe failed; cannot honour explicit request");
                    HwEncConfig::unmet("ffmpeg -encoders probe failed")
                }
                None => {
                    tracing::warn!(error = %e,
                        "hwenc: ffmpeg -encoders probe failed; defaulting to software");
                    HwEncConfig::lenient(HwEncoder::None)
                }
            };
        }
    };

    let Some(want) = want else {
        let pick = auto_pick(&listed);
        match pick {
            HwEncoder::None => tracing::info!("hwenc: none (no h264 hw encoder available)"),
            other => tracing::info!(encoder = other.ffmpeg_name(), "hwenc: detected"),
        }
        return HwEncConfig::lenient(pick);
    };

    match validate_explicit(want, &listed) {
        Ok(enc) => {
            tracing::info!(encoder = enc.ffmpeg_name(), "hwenc: detected (strict)");
            HwEncConfig::strict(enc)
        }
        Err(reason) => {
            tracing::error!(
                encoder = want.ffmpeg_name(),
                reason,
                "hwenc: explicit request unsatisfiable; transcodes will fail (direct/remux unaffected)"
            );
            HwEncConfig::unmet(reason)
        }
    }
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

/// `Err(reason)` when the request can't be honoured. The caller decides what
/// that means — never silently substitutes software, which is how a stale
/// ffmpeg build or a renumbered render node used to become a quiet outage.
fn validate_explicit(want: HwEncoder, listed: &EncoderSet) -> Result<HwEncoder, &'static str> {
    let ok = match want {
        HwEncoder::Vaapi => listed.vaapi,
        HwEncoder::Qsv => listed.qsv,
        HwEncoder::VideoToolbox => listed.videotoolbox,
        HwEncoder::None => true,
    };
    if !ok {
        return Err("encoder not present in this ffmpeg build");
    }
    #[cfg(target_os = "linux")]
    {
        if matches!(want, HwEncoder::Vaapi | HwEncoder::Qsv) && !has_dri_render_device() {
            return Err("no /dev/dri/renderD* device present");
        }
    }
    Ok(want)
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

#[cfg(test)]
mod tests {
    use super::*;

    const NOTHING: EncoderSet = EncoderSet { vaapi: false, qsv: false, videotoolbox: false };

    #[test]
    fn explicit_request_never_downgrades_silently() {
        // Used to warn and return `HwEncoder::None`, so a stale ffmpeg build
        // or a renumbered render node left the server transcoding on libx264
        // with only a startup warning to show for it.
        for want in [HwEncoder::Vaapi, HwEncoder::Qsv, HwEncoder::VideoToolbox] {
            assert!(
                validate_explicit(want, &NOTHING).is_err(),
                "{} should be unsatisfiable when absent from the ffmpeg build",
                want.ffmpeg_name()
            );
        }
    }

    #[test]
    fn videotoolbox_needs_no_device_node() {
        // The encoder owns its own session, so being listed is sufficient —
        // and this holds on every platform, unlike the VAAPI/QSV device gate.
        let listed = EncoderSet { videotoolbox: true, ..NOTHING };
        assert_eq!(validate_explicit(HwEncoder::VideoToolbox, &listed), Ok(HwEncoder::VideoToolbox));
    }

    #[test]
    fn auto_settles_on_software_when_nothing_is_listed() {
        assert_eq!(auto_pick(&NOTHING), HwEncoder::None);
    }
}

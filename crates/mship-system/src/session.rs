//! The host session's phone-specific settings, written before sway starts (mship-sway.service ExecStartPre), so the
//! sway config itself names no phone's parts: the panel's connector and scale, the touchscreen mapped to it, the gap
//! over a camera cutout, every other connector disabled (lent to VMs), and the KMS device for wlroots. Found with
//! mship_core::device; device.json panel.scale / panel.top_inset (px) refine them.

use std::time::{Duration, Instant};

use mship_core::device;

use crate::Res;

pub const SWAY_CONF: &str = "/run/mothership/sway/device.conf";
pub const SWAY_ENV: &str = "/run/mothership/sway/env";

/// The sway lines for this panel and these other connectors.
pub fn sway_conf(p: &device::Panel, others: &[String], top_inset: u64) -> String {
    let mut s = format!(
        "# written by mship-system session-setup (mship_core::device)\n\
         output {o} scale {scale} bg #242424 solid_color\n\
         input type:touch map_to_output {o}\n",
        o = p.output,
        scale = p.scale,
    );
    if top_inset > 0 {
        s += &format!("workspace terminal gaps top {top_inset}\n");
    }
    for o in others {
        s += &format!("output {o} disable\n");
    }
    s
}

/// Every connector but the panel's (an external display is lent to VMs, never sway's).
fn other_connectors(panel: &str) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir("/sys/class/drm")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            let (card, out) = n.split_once('-')?;
            (card.starts_with("card") && out != panel && !out.starts_with("Writeback")).then(|| out.to_string())
        })
        .collect();
    v.sort();
    v.dedup();
    v
}

pub fn setup() -> Res<()> {
    // the display controller registers seconds into the boot (after the GPU)
    let start = Instant::now();
    let panel = loop {
        if let Some(p) = device::panel() {
            break p;
        }
        if start.elapsed() > Duration::from_secs(30) {
            return Err("no built-in panel (DSI/eDP connector) after 30 s".into());
        }
        std::thread::sleep(Duration::from_millis(250));
    };
    let inset = device::conf()["panel"]["top_inset"].as_u64().unwrap_or(0);
    std::fs::create_dir_all("/run/mothership/sway").map_err(|e| e.to_string())?;
    std::fs::write(SWAY_CONF, sway_conf(&panel, &other_connectors(&panel.output), inset)).map_err(|e| format!("{SWAY_CONF}: {e}"))?;
    std::fs::write(SWAY_ENV, format!("WLR_DRM_DEVICES={}\n", panel.card)).map_err(|e| format!("{SWAY_ENV}: {e}"))?;
    println!("session: panel {} ({}x{}, {}), scale {}", panel.output, panel.width, panel.height, panel.card, panel.scale);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nio_session() {
        let p = device::Panel { output: "DSI-1".into(), card: "/dev/dri/card1".into(), width: 1080, height: 2520, refresh_hz: 90, dpi: 420, scale: 2.0 };
        assert_eq!(sway_conf(&p, &["DP-1".into()], 60),
                   "# written by mship-system session-setup (mship_core::device)\noutput DSI-1 scale 2 bg #242424 solid_color\n\
                    input type:touch map_to_output DSI-1\nworkspace terminal gaps top 60\noutput DP-1 disable\n");
    }
}

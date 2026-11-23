//! The phone's screen: the panel (its connector's DPMS: CRTC, panel and backlight), its touchscreen, and the screen flag
//! power reads (the phone may sleep once the screen has been off for the sleep delay). The power key toggles it
//! (`mship screen toggle`), swayidle switches it off after the screen timeout (`mship screen off`).

use mship_host::power as hpower;
use mship_host::util::Result;

use crate::handover::Handover;
use crate::session;

/// The panel's connector (mship_core::device).
fn panel_output() -> String {
    mship_core::device::panel_output()
}

/// Panel and touch only (the handover lends the panel to a VM with it on). Only the panel's touchscreen: synthetic
/// test devices stay live.
pub fn panel(on: bool) {
    let s = if on { "enabled" } else { "disabled" };
    let (out, touch) = (panel_output(), mship_core::device::touchscreen().map(|t| t.sway_id()).unwrap_or_default());
    if on {
        session::sway(&["output", &out, "power", "on"]);
        session::sway(&["input", &touch, "events", s]);
    } else {
        session::sway(&["input", &touch, "events", s]);
        session::sway(&["output", &out, "power", "off"]);
    }
}

pub fn set(handover: &Handover, on: bool) -> Result<()> {
    if on {
        hpower::set_screen(true)?;
        panel(true);
        handover.screen(true);
    } else {
        handover.screen(false); // no touches to a VM while the panel is off
        panel(false);
        hpower::set_screen(false)?;
    }
    Ok(())
}

/// Is the screen on? (sway's DPMS state; the saved flag if sway does not answer.)
pub fn is_on() -> bool {
    session::output_power(&panel_output()).unwrap_or_else(hpower::screen_on)
}

pub fn toggle(handover: &Handover) -> Result<bool> {
    let on = !is_on();
    set(handover, on)?;
    Ok(on)
}

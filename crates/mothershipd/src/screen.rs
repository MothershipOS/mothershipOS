//! The phone's screen: the panel (DSI-1 DPMS: CRTC, panel and WLED backlight), its touchscreen, and the screen flag
//! power reads (the phone may sleep once the screen has been off for the sleep delay). The power key toggles it
//! (`mship screen toggle`), swayidle switches it off after the screen timeout (`mship screen off`).

use mship_host::power as hpower;
use mship_host::util::Result;

use crate::handover::Handover;
use crate::session;

pub const PANEL: &str = "DSI-1";
/// only the panel touchscreen (synthetic test devices stay live)
const TOUCH: &str = "0:0:Novatek_NT36672C_Touchscreen";

/// Panel and touch only (the handover lends the panel to a VM with it on).
pub fn panel(on: bool) {
    let s = if on { "enabled" } else { "disabled" };
    if on {
        session::sway(&["output", PANEL, "power", "on"]);
        session::sway(&["input", TOUCH, "events", s]);
    } else {
        session::sway(&["input", TOUCH, "events", s]);
        session::sway(&["output", PANEL, "power", "off"]);
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
    session::output_power(PANEL).unwrap_or_else(hpower::screen_on)
}

pub fn toggle(handover: &Handover) -> Result<bool> {
    let on = !is_on();
    set(handover, on)?;
    Ok(on)
}

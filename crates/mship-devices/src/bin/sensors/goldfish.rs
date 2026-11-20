//! The mship.sensors 1.x guest protocol on cuttlefish-arm64 (docs/contracts/mship.sensors-1.md): the Android
//! emulator's sensor messages (what the guest's stock goldfish sensors HAL speaks), each in a Cuttlefish transport
//! frame: u32 command (bits 0-30) | response (bit 31), u32 payload size, payload. No I/O here.

/// The sensors the slot offers, by the HAL's handle (its bit in the `list-sensors` mask) and the name it enables them
/// by (`set:<name>:<0|1>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Sensor {
    Accel,
    Gyro,
    Mag,
    Proximity,
    Light,
    MagUncal,
    GyroUncal,
    AccelUncal,
}

impl Sensor {
    pub const ALL: [Sensor; 8] =
        [Sensor::Accel, Sensor::Gyro, Sensor::Mag, Sensor::Proximity, Sensor::Light, Sensor::MagUncal, Sensor::GyroUncal, Sensor::AccelUncal];

    pub fn handle(self) -> u32 {
        match self {
            Sensor::Accel => 0,
            Sensor::Gyro => 1,
            Sensor::Mag => 2,
            Sensor::Proximity => 5,
            Sensor::Light => 6,
            Sensor::MagUncal => 9,
            Sensor::GyroUncal => 10,
            Sensor::AccelUncal => 17,
        }
    }

    fn enable_name(self) -> &'static str {
        match self {
            Sensor::Accel => "acceleration",
            Sensor::Gyro => "gyroscope",
            Sensor::Mag => "magnetic-field",
            Sensor::Proximity => "proximity",
            Sensor::Light => "light",
            Sensor::MagUncal => "magnetic-field-uncalibrated",
            Sensor::GyroUncal => "gyroscope-uncalibrated",
            Sensor::AccelUncal => "acceleration-uncalibrated",
        }
    }

    fn event_name(self) -> &'static str {
        match self {
            Sensor::Mag => "magnetic",
            Sensor::MagUncal => "magnetic-uncalibrated",
            s => s.enable_name(),
        }
    }

    /// on change (proximity, light) or streaming
    pub fn on_change(self) -> bool {
        matches!(self, Sensor::Proximity | Sensor::Light)
    }
}

/// The `list-sensors` answer: the mask of handles offered.
pub fn mask() -> u32 {
    Sensor::ALL.iter().fold(0, |m, s| m | 1 << s.handle())
}

/// What the guest sends.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    ListSensors,
    Set(Sensor, bool),
    /// one rate for every sensor, as the interval between samples
    Delay(u32),
    /// the guest's clock (ignored: events are stamped on arrival)
    Time,
    Other(String),
}

pub fn command(payload: &[u8]) -> Command {
    let s = String::from_utf8_lossy(payload);
    let s = s.trim_end_matches('\0');
    if s == "list-sensors" {
        return Command::ListSensors;
    }
    if let Some(rest) = s.strip_prefix("set:")
        && let Some((name, on)) = rest.rsplit_once(':')
        && let Some(sensor) = Sensor::ALL.iter().find(|x| x.enable_name() == name)
    {
        return Command::Set(*sensor, on == "1");
    }
    if let Some(ms) = s.strip_prefix("set-delay:").and_then(|v| v.parse().ok()) {
        return Command::Delay(ms);
    }
    if s.starts_with("time:") {
        return Command::Time;
    }
    Command::Other(s.to_string())
}

/// One frame around a payload (command 0).
pub fn frame(payload: &str) -> Vec<u8> {
    let mut b = Vec::with_capacity(8 + payload.len());
    b.extend_from_slice(&0u32.to_le_bytes());
    b.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    b.extend_from_slice(payload.as_bytes());
    b
}

/// The payloads of the complete frames at the start of `buf` (removed from it). Err: an impossible size.
pub fn frames(buf: &mut Vec<u8>) -> Result<Vec<Vec<u8>>, ()> {
    let mut out = vec![];
    loop {
        if buf.len() < 8 {
            return Ok(out);
        }
        let n = u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize;
        if n > 4096 {
            return Err(());
        }
        if buf.len() < 8 + n {
            return Ok(out);
        }
        out.push(buf[8..8 + n].to_vec());
        buf.drain(..8 + n);
    }
}

/// A vector event (`acceleration:x:y:z`, ...).
pub fn vec3(s: Sensor, v: [f32; 3]) -> String {
    format!("{}:{}:{}:{}", s.event_name(), v[0], v[1], v[2])
}

/// A scalar event (`light:<lux>`, `proximity:<cm>`).
pub fn scalar(s: Sensor, v: f32) -> String {
    format!("{}:{}", s.event_name(), v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands() {
        assert_eq!(command(b"list-sensors"), Command::ListSensors);
        assert_eq!(command(b"set:magnetic-field:1"), Command::Set(Sensor::Mag, true));
        assert_eq!(command(b"set:acceleration-uncalibrated:0"), Command::Set(Sensor::AccelUncal, false));
        assert_eq!(command(b"set-delay:20"), Command::Delay(20));
        assert_eq!(command(b"time:123"), Command::Time);
        assert!(matches!(command(b"set:humidity:1"), Command::Other(_)));
        assert_eq!(mask(), 0b10_0000_0110_0110_0111);
    }

    #[test]
    fn framing() {
        let mut b = frame("list-sensors");
        b.extend(frame("set-delay:5"));
        b.extend(&frame("x")[..6]); // incomplete
        let f = frames(&mut b).unwrap();
        assert_eq!(f, vec![b"list-sensors".to_vec(), b"set-delay:5".to_vec()]);
        assert_eq!(b.len(), 6);
        assert_eq!(vec3(Sensor::Mag, [1.0, -2.5, 0.0]), "magnetic:1:-2.5:0");
        assert_eq!(scalar(Sensor::Proximity, 0.0), "proximity:0");
    }
}

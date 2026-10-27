//! mothershipOS host operations (run as root, by mothershipd): what the phone's settings and status pages need.
//! Every function returns the shapes in [`mship_core::status`] or a [`util::Error`] with a message for the user.

pub mod bt;
pub mod cpu;
pub mod power;
pub mod region;
pub mod resources;
pub mod ssh;
pub mod util;
pub mod vm;
pub mod wifi;

use mship_core::status::{GuardState, HostStatus, SshSummary};

/// The home screen's summary.
pub fn host_status(guard: Option<&GuardState>) -> HostStatus {
    let ssh = ssh::status();
    HostStatus {
        wifi: wifi::status(),
        owners: wifi::owners(),
        cpu: cpu::status(guard),
        brightness: power::brightness(),
        battery: power::battery_status(),
        ssh: SshSummary {
            enabled: ssh.conf.enabled,
            active: ssh.active,
            listen: ssh.conf.listen.clone(),
            addresses: ssh.addresses,
        },
    }
}

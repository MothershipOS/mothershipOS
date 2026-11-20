# Compatibility registry

The source of truth for what each mothershipOS release supports. Rules: [hardware-handles.md](hardware-handles.md) §3. The machine-
readable copy is `crates/mship-core/src/contracts.rs`. Tests check that both agree
(`crates/mship-core/tests/compat.rs`).

V1 (release 2026.1) is the baseline: nothing before it is covered.
Specs: [`contracts/`](contracts/README.md) (one file per contract major), `platforms/` (one per platform major).

## Release checklist
A release ships only when: every contract/platform file it introduces lists nothing under Needs work, Broken or
Not implemented, the test suite (`cargo xtask check`) passes including all frozen fixtures (`tests/compat/`), and
this file matches the code. 2026.1 is blocked by the items listed in `contracts/` and `platforms/`. Not registered yet: `mship.power`
(implemented on the host) and `mship.fingerprint` (design only).

## Releases
| Release | Date | Notes |
|---|---|---|
| 2026.1 | (in development) | V1: manifest schema 1, every contract and platform at 1.0 |

## Manifest schemas
| Schema | Introduced | Read by |
|---|---|---|
| 1 | 2026.1 | ≥ 2026.1 |

## Contracts
| Contract | Version | Introduced | Deprecated | Removed (not before) |
|---|---|---|---|---|
| mship.display | 1.0 | 2026.1 | — | — |
| mship.display-power | 1.0 | 2026.1 | — | — |
| mship.touch | 1.0 | 2026.1 | — | — |
| mship.keys | 1.0 | 2026.1 | — | — |
| mship.net-shared | 1.0 | 2026.1 | — | — |
| mship.gpu | 1.0 | 2026.1 | — | — |
| mship.wifi | 1.0 | 2026.1 | — | — |
| mship.bt | 1.0 | 2026.1 | — | — |
| mship.battery | 1.0 | 2026.1 | — | — |
| mship.usb | 1.0 | 2026.1 | — | — |
| mship.modem | 1.0 | 2026.1 | — | — |
| mship.camera | 1.0 | 2026.1 | — | — |
| mship.audio | 1.0 | 2026.1 | — | — |
| mship.gnss | 1.0 | 2026.1 | — | — |
| mship.sensors | 1.0 | 2026.1 | — | — |

## Platforms
| Platform | Version | Introduced | Deprecated | Removed (not before) |
|---|---|---|---|---|
| cuttlefish-arm64 | 1.0 | 2026.1 | — | — |
| efi-virt-arm64 | 1.0 | 2026.1 | — | — |

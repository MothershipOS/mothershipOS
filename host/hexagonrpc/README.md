# hexagonrpc

hexagonrpcd, the FastRPC file server for Qualcomm DSPs, built from the pinned upstream release (`source.json`) with
`patches/` (`cargo xtask hexagonrpc`, part of `cargo xtask dist`): static, installed as
`/usr/local/lib/mothership/hexagonrpcd` and run by `mship-sensors` for the SLPI's sensor process. Ubuntu's own
`hexagonrpcd` package is not used (its service attaches to the ADSP; its server is read-only).

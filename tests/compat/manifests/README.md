# Frozen manifest fixtures

`v1/*.json` are schema-1 manifests as V1 (mothershipOS 2026.1) defines them. **From the 2026.1 release on, never edit
or delete them** (until then V1 is still being designed and they follow the design).
Every future release must load each one, report it runnable (or, for `forward-compatible.json`, runnable with
the unknown optional slot left empty) and keep its meaning. When a new schema ships, add `v2/` next to it.
`expected.json` pins what the loader must report for each fixture. `invalid/` holds files that must be
rejected; the file name says why.

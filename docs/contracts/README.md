# Slot contracts

One file per contract and major version (`<contract>-<major>.md`). A contract states what the **guest** can rely
on, in terms of devices and data the guest sees. Host internals (backends, VMMs, sockets) are not part of it.
Rules for changing a contract: `../hardware-handles.md` §3. Registry: `../compat.md`.

Every contract file has the same parts: guest view, caps, attached/empty behaviour, minor-version history, and
the conformance gaps of the current implementation (must be empty before the release that introduces the
version ships).

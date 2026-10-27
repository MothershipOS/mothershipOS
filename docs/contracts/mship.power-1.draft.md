# mship.power 1.x — draft system-power handle

Status: **planning; the request part is implemented as [mship.power 1.0](mship.power-1.md) (2026-10-25)**. Prepared 2026-10-13.
This draft records the proposed interface and implementation constraints. Profile/sleep semantics and
implementation choices remain open for review; no sleep behavior is enabled by this document.

## Fit with existing handles

Use the [hardware-handle model](../hardware-handles.md), its manifest slots, version negotiation,
capability discovery, attachment lifecycle, release reasons, and compatibility rules. Power is a shared
host capability: the physical system has one power state, with independent requests from each running VM.
It is not an exclusively lent device, and it does not move with screen ownership.

| Item | Proposed definition |
|---|---|
| Handle | `power:system` |
| Contract | `mship.power`, draft major 1 |
| Manifest slot | `system_power` |
| Sharing | Shared; one attachment and request per running VM |
| Group | None; not a user-lendable hardware group |
| Lifetime | VM start through stop/crash; independent of show/take-screen |
| Host authority | The broker owns the handle, participation set, and aggregate policy |
| Guest authority | Request its own needs and observe the effective state |
| Physical backend | Host-only CPU/device power policy, suspend, and wake-source preparation |

The existing slot named `power` uses `mship.display-power`. Keep it and that contract unchanged.
Display state and permission to suspend are different inputs.

Proposed new slot fragment (illustration only; do not add to runnable packages before support exists):

```json
"system_power": {
  "contract": "mship.power",
  "version": "1.0",
  "required": true
}
```

Reuse schema 1's existing slot structure and required-contract validation. An old host that cannot serve
the required contract refuses the new package with the existing compatibility diagnostics. Existing
packages without the slot continue to work, but their running VMs prevent automatic host suspend.
New packages intended to participate must include a guest adapter and declare this slot as required.
Retrofitting older packages is a separate decision; do not rewrite manifests implicitly.

## Request semantics proposed for review

Each guest adapter consolidates its OS's needs into one atomic request:

| Field | Meaning |
|---|---|
| `profile` | `powersave`, `balanced`, or `performance`; minimum requested awake-system profile |
| `may_sleep` | Permission to consider physical suspend; false until explicitly set |
| `reason` | Bounded diagnostic text; never an identity or authorization mechanism |
| `request_seq` | Increasing sequence within the current attachment, used to reject stale messages/readiness |

The host maintains its own profile and suspend inhibitors alongside the VM requests.

```text
awake profile = max(host profile, every running VM profile)
                 where powersave < balanced < performance

sleep eligible = every running VM has a valid attached request with may_sleep = true
                 AND the host permits suspend
```

`performance` versus `powersave` therefore selects `performance` while the host is awake. A power-saving
guest can continue work and deny sleep. Sleep permission is proposed as a separate field because lower
CPU power does not imply that an operation may be paused. A performance preference with explicit sleep
permission is a profile preference for awake operation, not an unconditional sleep veto.

No guest directly selects CPU frequencies, writes host power sysfs nodes, or programs physical hardware.
Advertised capabilities describe supported abstract profiles and wake mechanisms. The backend maps them
to the port's hardware and existing host policy. The host reports requested versus applied behavior and
any limit/blocker; user, thermal, and battery constraints can limit performance but must not silently
turn a guest's keep-awake requirement into suspend permission.

A guest's resource budget is separate from the aggregate hardware profile. Keeping the shared host in
performance mode does not automatically grant every VM a larger CPU budget.

## Channel and compatibility

- Use a platform-provided channel slot. Vsock is the candidate transport where the platform exposes it;
  choose and document the endpoint in both platform contracts before implementation. CID allocation
  remains broker-owned, and guests do not carry device-specific endpoints or fixed VM identities.
- The first exchange follows existing channel negotiation: `hello {contract, versions}`, then the highest
  common major/minor or `unsupported`. Reuse `mothershipd.versions.negotiate` on the host.
- Reuse existing request IDs, result/error conventions, `attached`/`detached` events, and release reasons.
  Identify the VM from its broker-issued channel/connection, never a VM name claimed in a payload.
- Select framing before freezing 1.0. The implemented Wi-Fi channel uses little-endian `<IHHI>` headers
  and aligned TLV attributes, while its document currently describes JSON framing. Review that existing
  mismatch; do not copy its documented JSON encoding and assume it is what the implementation uses.
  Prefer a compatible shared framing helper with protocol-specific operations, preserving Wi-Fi's wire
  bytes. No new standalone transport/discovery/lifecycle framework should be introduced for power.
- Minor versions add optional fields/messages only. Apply the existing major support window, adapter
  rules, and frozen compatibility fixtures when this contract ships.

Conceptual operations (not final wire IDs or JSON framing):

| Direction | Operation | Purpose |
|---|---|---|
| Guest → host | `hello` | Contract/version negotiation |
| Guest → host | `set_request` | Atomically replace this attachment's profile, permission, and sequence |
| Guest → host | `get_state` | Read supported capabilities and effective state |
| Host → guest | `result` | Request ID, success or error, and accepted request sequence |
| Host → guest | `attached`, `detached` | Existing attachment/release semantics |
| Host → guest | `state_changed` | Requested aggregate, applied profile, and relevant host blockers |
| Host → guest | `prepare_sleep` | Ask for readiness for a specific preparation generation |
| Guest → host | `ready`, `not_ready` | Readiness tied to that generation and the current request sequence |
| Host → guest | `sleep_cancelled`, `resumed` | End preparation or require a fresh request after wake |

Do not expose another VM's private diagnostic text to guests by default. The host UI may show per-VM
requests and blockers through the broker's existing management/watch interface.

## Shared attachment and release

Each VM's attachment uses `empty → attached → releasing → empty`. Releasing one attachment does not
detach the shared handle from other VMs. Shared attachment bookkeeping belongs to the broker.

- VM start: add the VM to the participation set immediately. Until negotiation, attachment, and a valid
  request succeed, it is unknown: keep awake and use a conservative profile fallback. Proposed fallback
  is `performance`; confirm it during the decision session.
- Attach: send capabilities and `attached`; start with fresh request state. No vote/readiness from an old
  connection or VM incarnation is reused.
- Disconnect, malformed required messages, adapter failure, or explicit revocation while the VM still
  runs: invalidate its request/readiness and keep that VM as an unknown awake participant.
- Release is immediate, without waiting for guest cooperation. Send best-effort `detached {reason}`,
  invalidate state, and close the channel using the normal handle cleanup path.
- Stop/crash of the VM itself: lifecycle confirmation removes it from the participation set. Adapter
  disconnection alone does not prove that the VM stopped.
- Host/broker restart: rebuild participation from authoritative running-VM state; reconnect with fresh
  requests. Never restore sleep permission from a persisted diagnostic snapshot.
- Zero running VMs: eligibility follows host activity/inhibitors. Absence of VM votes does not itself
  demand suspend.

Periodic heartbeats, if selected, must not create wakeups during physical sleep or expire an intentionally
quiesced guest into a false failure. Connection health and readiness cannot rely on wall-clock polling alone.

## Preparation, cancellation, and wake

Unanimous permission is eligibility, not immediate permission to write `/sys/power/state`.

1. Capture a preparation generation covering participants, attachments, and their accepted request sequences.
2. Send `prepare_sleep` to each eligible guest adapter. It quiesces relevant guest activity and responds
   `ready` or `not_ready` for that generation. Missing/late responses cancel preparation; timeout is TBD.
3. A new VM, changed request, disconnect, host inhibitor, or participant mismatch invalidates preparation.
   A guest needing new awake work must withdraw permission and receive host acceptance before starting it.
4. The host backend prepares virtual/physical devices and supported wake sources. Verify that pending guest
   channel work and physical wake events cannot race the final eligibility check; use the kernel's
   wakeup-count mechanism as appropriate. Exact VMM quiescing/commit boundaries require design and tests.
5. Suspend only after final revalidation. On failure or cancellation, release preparation and notify guests.
6. After resume, restore devices/VMM operation and send `resumed`. Require fresh requests/readiness before
   another suspend; restore the chosen awake-profile policy during that transition.

Timed wake deadlines are required before a guest with scheduled work can safely grant permission. Clock
domain, deadline conversion, cancellation, wake latency, and unsupported wake-source behavior remain TBD.
If the host cannot satisfy an outstanding deadline or required wake event, keep awake and report the blocker.

## Implementation boundaries

The version/manifest/contract core exists under `host/mothershipd/mothershipd/`. Its handle registry and
backend dispatch are still planned. Build power on that broker lifecycle rather than adding an independent
VM list, ownership registry, launcher-only vote files, or process-name policy beside it.

- **Broker core:** authoritative participants and per-attachment state; shared handle attach/release;
  aggregate requests and existing management/watch reporting.
- **Power handle backend:** the same planned `attach(vm, slot)`, `detach()`, `revoke()`, `caps()` interface as
  other classes, scoped to each shared attachment. Central aggregation is independent of port hardware.
- **Host actuator:** adapt existing `host/power/` code to consume the broker's approved policy. Physical
  suspend/frequency/device actions stay behind the backend, with one authority for each action.
- **Platform setup:** provide the channel for both crosvm and QEMU; transport identity comes from broker
  allocations. No guest dependency on the selected VMM.
- **Guest packages:** a common contract implementation with OS-specific adapters. Android wakelock/idle
  integration and Linux activity/suspend integration stay within their respective guest adapters; their
  OS daemon APIs are not the cross-VM contract.
- **Compatibility:** register `mship.power` and update `docs/compat.md`, manifests, and platform contracts
  together only once the implementation is actually supported. A draft alone is not support.

## Checklist before implementation

- [ ] Confirm profile names, independent sleep permission, and conservative fallback.
- [ ] Specify shared-control attachment bookkeeping in the broker and descriptor schema.
- [ ] Choose transport, framing, endpoint, limits, and guest-visible capability fields; reconcile the Wi-Fi framing discrepancy.
- [ ] Define preparation/commit boundaries, cancellation, wake scheduling, and resume behavior.
- [ ] Choose how new packages require the slot and how existing packages acquire adapters.
- [ ] Confirm the host policy/actuator boundary and the relationship to display power.

## Validation to prepare

- [ ] Replay contract hello/version, request/result, and event fixtures through the host and guest codecs.
- [ ] Verify new required-slot validation while unchanged manifests retain their existing behavior.
- [ ] Verify performance wins over balanced/powersave; any awake/unknown participant denies suspend.
- [ ] Verify all eligible VMs still cannot suspend over a host inhibitor or unsupported wake requirement.
- [ ] Exercise attachment/revocation, guest adapter disconnect, VM crash, and broker restart independently.
- [ ] Reject stale request/readiness messages across reconnect, VM restart, and preparation generations.
- [ ] Exercise a late veto, new VM, in-flight operation, pending wake event, and device/VMM preparation failure.
- [ ] Verify multiple guests' timed wakes and fresh participation after resume.
- [ ] Verify screen handover does not move/drop a VM's system-power attachment.
- [ ] Verify shared codec changes preserve existing Wi-Fi wire transcripts.
- [ ] Keep physical suspend disabled through protocol/aggregation tests; enable an actuator only in a separately selected implementation task.

Related investigation entries: [standby checklist](https://github.com/MothershipOS/motorola_nio/blob/main/docs/standby-checklist.md), especially S06–S12.
All implementation decisions remain pending.

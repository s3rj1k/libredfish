# `hw.rn` cURL cheat sheet

What each override in [`hw.rn`](./hw.rn) does and why, one cURL example per gap.
`hw.rn` targets a **Dell iDRAC**, where the gaps are Oem-only paths and firmware
that rejects standard Redfish properties, rather than the missing features
[`sushy.rn`](./sushy.rn) works around. Every method not listed here is
`[standard fallback]`: `hw.rn` doesn't define it, so `RedfishStandard`'s normal
behavior runs untouched (see `README.md`).

| flag | meaning |
|------|---------|
| `[rune: override]` | hw.rn defines this function; the cURL shown is the real gap it plugs. |
| `[standard fallback]` | not defined in hw.rn; RedfishStandard's normal request runs as is. |

Every override is gated on `is_dell`, which reads the Manager's `Oem` keys. On a
non-Dell BMC each one either returns an error naming the method or falls through
to the standard path, so the file is safe to point at the wrong machine. It
refuses rather than writing Dell attributes somewhere they don't belong.

## Connection

```bash
BMC='https://<idrac>'; U='<user>'; P='<pass>'
SYS=$(curl -sk -u "$U:$P" "$BMC/redfish/v1/Systems" | jq -r '.Members[0]."@odata.id"' | xargs basename)
MGR=$(curl -sk -u "$U:$P" "$BMC/redfish/v1/Managers" | jq -r '.Members[0]."@odata.id"' | xargs basename)
```

## Vendor detection `[rune: override helper]`

The Manager advertises the vendor under `Oem`, matched case-insensitively and
re-read per call because scripts hold no state:

```bash
curl -sk -u "$U:$P" "$BMC/redfish/v1/Managers/$MGR" | jq '.Oem | keys'
# => [ "Dell" ]
```

## UEFI HTTP boot: machine_setup / is_bios_setup / machine_setup_status `[rune: override]`

iDRAC has no working `UefiHttp` boot *device*, so HTTP boot is configured as BIOS
attributes instead. The script writes the smallest set that works, staged with
`ApplyTime: OnReset`:

```bash
curl -sk -u "$U:$P" "$BMC/redfish/v1/Systems/$SYS/Bios" \
  | jq '.Attributes | with_entries(select(.key | startswith("HttpDev1")))'
```

Three values are read from the firmware rather than hard-coded, so the wording
stays whatever this firmware calls it:

```bash
# Interfaces this BIOS accepts for HttpDev1Interface, and the TLS-off spelling.
curl -sk -u "$U:$P" "$BMC/redfish/v1/Systems/$SYS/Bios/BiosRegistry" \
  | jq '.RegistryEntries.Attributes[]
        | select(.AttributeName | IN("HttpDev1Interface","HttpDev1TlsMode"))
        | {(.AttributeName): [.Value[].ValueName]}'
```

`HttpDev1TlsMode` must be the off value or the firmware answers `UEFI0417` and
refuses plain HTTP. `HttpDev1Uri` is only written when the override data supplies
one; without it the firmware has nothing to fetch and drops to the next boot
device (DHCP supplies the address, not the URI).

The boot NIC comes from `http_boot_nic` in the override `data`, or is discovered
as a `LinkUp` port the registry also accepts. When neither yields one,
`machine_setup` does nothing and `machine_setup_status` reports a `boot_slot`
diff rather than guessing.

## Boot order: is_boot_order_setup / set_boot_order_dpu_first `[rune: override]`

`SetBootOrderEn` only *enables* devices; it does not sequence them. The order is a
patch to `Systems/$SYS/Settings`, and the HTTP entry is found by display name:

```bash
curl -sk -u "$U:$P" "$BMC/redfish/v1/Systems/$SYS/BootOptions?\$expand=.(\$levels=1)" \
  | jq -r '.Members[] | "\(.Id)  \(.DisplayName)"'
# => Boot0000  HTTP Device 1: NIC in Slot 5 Port 1

curl -sk -u "$U:$P" "$BMC/redfish/v1/Systems/$SYS" | jq '.Boot.BootOrder'
```

Netboot goes first, then the volume this machine boots from, so a stale install on
another disk cannot win the fallback. Both matches are overridable through the
override `data` (`http_boot_entry_match`, `boot_volume_entry_match`).

Two traps the script works around: a committed pending config makes the patch fail
with `SYS011`, so the job queue is cleared first; and without an explicit
`ApplyTime` the patch is accepted, staged as `Deferred`, and never scheduled, so
the caller gets no job, never reboots, and its verify reads the old order forever.

## boot_once / boot_first / set_boot_override `[rune: override]`

iDRAC returns `PropertyNotWritable` for `Boot.BootSourceOverrideTarget` and
`BootSourceOverrideEnabled`, so the standard path silently does nothing. The first
boot device is a Manager attribute instead, and it applies immediately. Asking
for `OnReset` here makes the firmware report success while staging nothing:

```bash
ATTRS="$BMC/redfish/v1/Managers/$MGR/Oem/Dell/DellAttributes/$MGR"
curl -sk -u "$U:$P" "$ATTRS" \
  | jq '.Attributes | with_entries(select(.key | test("^ServerBoot\\.")))'
# => { "ServerBoot.1.FirstBootDevice": "Normal", "ServerBoot.1.BootOnce": "Enabled" }
```

`FirstBootDevice` takes `Normal, PXE, HDD, BIOS, FDD, SD, F10, F11, UefiHttp`.
`set_boot_override` is only meaningful with an HTTP boot URI here and errors
otherwise, rather than reporting a success that changed nothing.

## Lockdown: lockdown_status / lockdown_bmc / set_idrac_lockdown `[rune: override]`

Lockdown blocks every provisioning write, and the standard `lockdown_bmc` is a
no-op that reports success, so the host stayed locked and every write after it
failed. Lockdown is only fully on when racadm is off alongside it, so a
half-applied pair reports `Partial` rather than a confident wrong answer:

```bash
curl -sk -u "$U:$P" "$ATTRS" \
  | jq '.Attributes | with_entries(select(.key | test("Lockdown|^Racadm\\.[0-9]+\\.Enable$")))'
# => { "Lockdown.1.SystemLockdown": "Disabled", "Racadm.1.Enable": "Enabled" }
```

Dell numbers its attribute groups and the index is not guaranteed to be `1`, so
the live key is matched on group and field rather than assuming `.1.`. Enabling
sends racadm-off first and lockdown on its own, because lockdown applies at once
and would reject anything sent alongside it.

## Jobs: get_job_state `[rune: override]`

Dell keeps jobs under its own Oem path, which the standard implementation cannot
read:

```bash
curl -sk -u "$U:$P" "$BMC/redfish/v1/Managers/$MGR/Oem/Dell/Jobs" | jq -r '.Members[]."@odata.id"'
curl -sk -u "$U:$P" "$BMC/redfish/v1/Managers/$MGR/Oem/Dell/Jobs/<JID>" | jq -r '.JobState'
```

Job ids come back in the `Location` header of a settings patch, not the body.
Dell also reports states outside the `JobState` enum, so anything unrecognised
degrades to `unknown` instead of failing the call.

`JID_CLEARALL` drops config jobs but leaves the `RID_` reboot jobs behind, so
those are deleted one at a time; deleting everything would also take out a config
job the caller just created and is waiting on.

## Storage: get_boss_controller / decommission_storage_controller / create_storage_volume `[rune: override]`

Without `get_boss_controller` the caller falls back to a generic NVMe wipe, which
the BOSS card rejects with an invalid opcode:

```bash
curl -sk -u "$U:$P" "$BMC/redfish/v1/Systems/$SYS/Storage" | jq -r '.Members[]."@odata.id"'
```

Both write paths refuse unless the lifecycle controller is `Ready`; a storage job
posted while it is busy is accepted and then never runs:

```bash
curl -sk -u "$U:$P" -X POST -H 'Content-Type: application/json' -d '{}' \
  "$BMC/redfish/v1/Managers/$MGR/Oem/Dell/DellLCService/Actions/DellLCService.GetRemoteServicesAPIStatus" \
  | jq -r '.LCStatus'
```

Volume creation mirrors one drive as `RAID0` and two as `RAID1`, and errors on any
other count rather than guessing a shape this card is not used in. A controller
that cannot apply a RAID job while the host runs (`RealtimeCapability` other than
`Capable`) gets a restart so the staged job drains during POST.

## change_uefi_password / clear_uefi_password `[rune: override]`

Dell calls it `SetupPassword` and needs an explicit BIOS config job. The standard
path sends `AdministratorPassword` with no job, which this firmware rejects. The
script unlocks first, because the caller only reaches its own unlock step once and
a retry would otherwise hit `SYS406` forever.

## set_ntp_servers `[rune: override]`

Read-only by design. The BMC holds its own NTP config and NICo must not overwrite
it, so this reads `NetworkProtocol` and accepts whether or not servers are set:

```bash
curl -sk -u "$U:$P" "$BMC/redfish/v1/Managers/$MGR/NetworkProtocol" | jq '.NTP'
```

## bmc_reset / bmc_reset_to_defaults `[rune: override]`

Accept and do nothing. Preingestion opens with a one-shot BMC reset that takes the
iDRAC away for minutes, and a factory reset would drop the BMC's static address
and credentials, leaving it reachable only from the physical console.

## Error-tolerant reads `[rune: override]`

`pcie_devices`, `get_accounts`, `get_component_integrities`, `get_drives_metrics`,
`get_collection` and `get_boot_options` return an empty result instead of
propagating an error, because a script error is fatal to callers such as
`fetch_pcie_devices` where a missing collection is not.

## get_system_ethernet_interface `[rune: override]`

Uses the `UefiDevicePath` the BMC reports and falls back to a MAC-keyed lookup in
the override `data` only when it is missing or blank, the same shape `sushy.rn`
uses:

```json
{ "uefi_device_path_by_mac": { "aa:bb:cc:dd:ee:ff": "PciRoot(0x0)/Pci(0x1,0x0)/..." } }
```

## is_infinite_boot_enabled `[rune: override]`

`BootSeqRetry` is the Dell name for retrying the boot sequence forever:

```bash
curl -sk -u "$U:$P" "$BMC/redfish/v1/Systems/$SYS/Bios" | jq -r '.Attributes.BootSeqRetry'
```

## Rune gotcha worth knowing

`len()` returns an unsigned value that never matches a signed integer literal in a
`match` arm, so a drive count compared that way silently falls through to the
fallback arm. `create_storage_volume` counts by hand for that reason.

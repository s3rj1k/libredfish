# `sushy.rn` cURL cheat sheet

What each override in [`sushy.rn`](./sushy.rn) does and why, one cURL example per
gap. `sushy.rn` targets **vanilla** (unpatched) OpenStack `sushy-tools`, not the
`s3rj1k/sushy-tools` fork some of the derivations below still reference for parity.
Every method not listed here is `[standard fallback]`, sushy.rn doesn't define it,
so `RedfishStandard`'s normal behavior runs untouched (see `README.md`).

| flag | meaning |
|------|---------|
| `[rune: override]` | sushy.rn defines this function; the cURL shown is the real gap it plugs. |
| `[standard fallback]` | not defined in sushy.rn; RedfishStandard's normal request runs as is. |

sushy has no DPU, so the DPU-only methods (`enable_rshim_bmc`, `set_host_rshim`,
`get_host_rshim`, `set_nic_mode`, `get_nic_mode`, `set_host_privilege_level`,
`get_base_mac_address`, ...) need no override either. `sushy.rn` doesn't define
them, and some (`set_host_rshim`, `set_nic_mode`, `set_host_privilege_level`)
can't be overridden from a script at all (see `README.md`'s "Always delegate"
table), so they all just return `RedfishStandard`'s own `NotSupported`/`Ok(None)`
defaults.

## Connection

```bash
BMC='https://<sushy-host>:<port>'; U='<user>'; P='<pass>'
SYS=$(curl -sk -u "$U:$P" "$BMC/redfish/v1/Systems" | jq -r '.Members[0]."@odata.id"' | xargs basename)
```

## No-op overrides `[rune: override]`

`machine_setup`, `is_bios_setup`, `set_ntp_servers` make no HTTP call at all. sushy
has no BIOS profile and no NTP backend to configure, so these just report success
directly.

## is_boot_order_setup `[rune: override]`

sushy stores a `UefiHttp` boot override as `Pxe`, so the override GETs the System
and reports configured only when the target is either value:
```bash
curl -sk -u "$U:$P" "$BMC/redfish/v1/Systems/$SYS/" | jq '.Boot.BootSourceOverrideTarget'
```
Reporting `true` unconditionally leaves `machine_setup_status` with no `boot_first`
diff, so `set_boot_order_dpu_first` never runs and the host never gets told to
network boot.

## get_software_inventories `[rune: override]`

Vanilla sushy 404s `UpdateService/FirmwareInventory`:
```bash
curl -k -u "$U:$P" "$BMC/redfish/v1/UpdateService/FirmwareInventory"   # => 404
```
The override returns `[]` directly, no request.

## get_system `[rune: override]`

Vanilla sushy's System has no `SerialNumber`:
```bash
curl -sk -u "$U:$P" "$BMC/redfish/v1/Systems/$SYS/" | jq '.SerialNumber'   # => null
```
The override GETs the System and, when blank, synthesizes one from `UUID`
(hyphens stripped, uppercased), matching the `s3rj1k/sushy-tools` patch's own
derivation.

## get_accounts / get_component_integrities / pcie_devices / get_drives_metrics / get_collection `[rune: override]`

Vanilla sushy ignores `$expand`, so `Members` stay shallow refs instead of full
resources:
```bash
curl -sk -u "$U:$P" "$BMC/redfish/v1/AccountService/Accounts?\$expand=.(\$levels=1)" | jq '.Members[0]'
# => { "@odata.id": "/redfish/v1/AccountService/Accounts/1" }
```
All five overrides call `ctx.expand_collection(path)` (a host function in
`src/rune_vendor.rs`), which tries `$expand` once, then falls back to GETting
each Member individually when the server ignored it. `pcie_devices` also
replicates the standard id/manufacturer/enabled-status filter and manufacturer
sort; `get_drives_metrics` also fetches each non-USB Drive ref found under the
expanded Storage members; `get_collection` is the generic catch-all for any other
direct collection fetch a script or caller makes.

`pcie_devices` additionally returns `[]` rather than propagating an error when the
collection cannot be fetched. Vanilla sushy 404s `PCIeDevices` on every chassis and
its chassis id is not the system id, and a script error is fatal to the caller's
`fetch_pcie_devices` rather than being tolerated the way a 404 is elsewhere:
```bash
curl -sk -u "$U:$P" "$BMC/redfish/v1/Chassis/$SYS/PCIeDevices"   # => 404
```

## set_boot_override / boot_once / boot_first / set_boot_order_dpu_first `[rune: override]`

One `PATCH /Systems/{id}` with a `Boot` object; the target string comes from
`boot_target_str` in `src/rune_vendor.rs` (`Pxe`, `Hdd`, `UefiHttp`); enabled is
`Once` (`boot_once`) or `Continuous` (`boot_first`):
```bash
curl -k -u "$U:$P" -X PATCH "$BMC/redfish/v1/Systems/$SYS" \
  -H 'Content-Type: application/json' \
  -d '{"Boot":{"BootSourceOverrideTarget":"UefiHttp","BootSourceOverrideEnabled":"Once"}}'
```
sushy has no DPU, so "boot order" just means which network boot protocol to
prefer: `set_boot_order_dpu_first` tries `boot_first(UefiHttp)`, falling back to
`boot_first(Pxe)` only if the BMC rejects the first PATCH.

## get_boot_options `[rune: override]`

Vanilla sushy has no `/Systems/{id}/BootOptions` endpoint at all:
```bash
curl -k -u "$U:$P" "$BMC/redfish/v1/Systems/$SYS/BootOptions"   # => 404
```
The `s3rj1k/sushy-tools` patch adds one that's always empty, so the override
returns that same shape directly, no request needed.

## get_system_ethernet_interface `[rune: override]`

Vanilla sushy's `EthernetInterface` has no `UefiDevicePath` (read by site
explorer's primary NIC pick):
```bash
curl -sk -u "$U:$P" "$BMC/redfish/v1/Systems/$SYS/EthernetInterfaces/1" | jq '.UefiDevicePath'   # => null
```
After the real GET, if it's still missing, the override looks up the interface's
`MACAddress` in `ctx.vendor_data()`'s optional `uefi_device_path_by_mac` table
(set in the vendor override file's `data` field) and fills it in when found.

---

_Source: `sushy.rn`; behavior compared against a vanilla (unpatched) OpenStack
`sushy-tools` checkout. Everything not listed above is undefined in the script and
runs `RedfishStandard`'s own implementation, cataloged in `README.md`._

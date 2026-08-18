# Rune vendor, what a script can use

The `rune` vendor (`src/rune_vendor.rs`) runs a [Rune](https://rune-rs.github.io)
script as a BMC backend. This is the reference for everything a script can call,
override, and rely on.

> **Trust:** scripts run with the host process's privileges, no sandbox, and no
> resource limits. `read_file`/`read_env` reach the real filesystem and
> environment. Only load scripts you trust.

## Selecting a script

Set `LIBREDFISH_VENDOR_OVERRIDE_FILE` to a JSON file that pins the `Rune` vendor
and points at the script, keyed by BMC address (and optionally manager id):

```json
[
  { "addr": "10.42.0.5", "vendor": "Rune", "script": "/etc/bmc.rn", "variant": "model-x",
    "data": { "uefi_device_path_by_mac": { "aa:bb:cc:dd:ee:ff": "..." } } }
]
```

The same file pins any vendor, not just `Rune`. An entry like
`{ "addr": "10.42.0.6", "manager": "1", "vendor": "Dell" }` forces the Dell
implementation with no script involved, which is what `src/vendor_override.rs`
documents by pointing here. See [Precedence](#precedence) for how a matched entry
ranks against a caller-supplied and an auto-detected vendor.

`variant` is optional free-form text the script reads via `ctx.variant()`.

`data` is an optional, unvalidated JSON value the script reads via
`ctx.vendor_data()`. libredfish doesn't interpret it at all, a script gives it
whatever shape it needs and looks it up however it likes (see `sushy.rn`'s
`get_system_ethernet_interface` for a MAC-address-keyed lookup example).

The override file is re-read and re-evaluated on every `get_service_root()` call,
not cached or frozen at client construction, so an edit to the file takes effect
on the BMC's next call rather than requiring a new client. As a result it's also
read more than once during client bootstrap; this is a known, accepted tradeoff.

The compiled script itself is cached by path and mtime, so a script edited twice
within the same mtime tick (coarse on some filesystems, e.g. 1-second resolution)
can keep serving the previous version for that one tick. Not a concern for the
typical deployment (e.g. Kubernetes), where a script update ships as a new pod.

### Precedence

A matching entry has the last word on the vendor. It beats the vendor a caller
passed to `create_client_with_vendor` and the one auto-detected from the service
root, and nothing probed off the BMC refines it afterwards: an entry naming `AMI`
stays `AMI` instead of being promoted to `LenovoGB300`, and one naming the `P3809`
placeholder stays unresolved rather than becoming `NvidiaGH200`/`NvidiaGBSwitch`.
An entry therefore has to name the vendor it actually wants.

An entry naming both `addr` and `manager` wins over an `addr`-only entry, which
acts as the default for every manager at that address. The manager id used as the
key is the one libredfish settles on, which on multi-system hosts is the manager
of the system that carries the BIOS rather than the first manager listed.

If `LIBREDFISH_VENDOR_OVERRIDE_FILE` is set but the file is missing, unreadable, or
malformed, client creation fails instead of silently falling back to auto-detection.
Failing closed keeps a typo from silently selecting the vendor you excluded.

## `system_id` / `manager_id` resolution

Going through `RedfishClientPool::create_client(_with_vendor)` already resolves
`system_id`/`manager_id` before selecting a vendor, so the common case needs nothing
extra. If a `Rune` client is instead built directly against `RedfishStandard` (e.g.
via `create_standard_client`) without those ids set, the vendor resolves them itself
the first time any method is dispatched, using the same rules the pool applies to
every other vendor:

- Prefer the canonical host system `System_0`, falling back to the first member,
  since some platforms list an auxiliary system such as an NVIDIA
  `HGX_Baseboard_0` ahead of the real host.
- Probe that preferred id for a `Bios` resource first, then the remaining members
  in enumeration order, and take the first system that has one. An auxiliary
  system can also advertise a `Bios`, so probing in bare enumeration order would
  discard the `System_0` preference. If no member exposes a `Bios`, the preferred
  id stands.
- Follow the chosen system's first `Links.ManagedBy` entry for `manager_id`,
  falling back to the first member of `Managers`. A `manager_id` the caller set
  explicitly is never moved by the probe.

The ordering helper (`system_ids_for_bios_probe`) is shared with the pool, so the
two paths cannot drift. Resolution runs at most once per client and is invisible to
scripts. `ctx.system_id()`/`ctx.manager_id()` see the resolved values either way.

This resolution requires at least one entry in `Systems`, even for a dispatch that
never references `ctx.system_id()`. A systems-less BMC (no `/Systems` at all) isn't
supported by the Rune vendor today, unlike the hand-written vendors that skip
system-id resolution entirely for such platforms (e.g. Delta power shelves).

## How a script hooks in

For each `Redfish` trait method, the vendor looks for a **top-level function with
the same name**. If the script defines it, it is called; otherwise the call falls
back to the standard Redfish implementation. So a script only implements what it
needs to change.

```rune
// Overrides get_power_state; everything else uses the standard behavior.
pub async fn get_power_state(ctx) {
    match ctx.get(`Systems/${ctx.system_id()}`).await {
        Ok(resp) => resp["body"]["PowerState"],
        Err(_) => "Unknown",
    }
}
```

Override functions should be `pub async fn` and take `ctx` first, followed by any
string arguments the method passes (see the tables below).

### Return & error conventions

A function's return value is bridged into the method's Rust return type via JSON,
so return shapes that match the type (a string for `PowerState`, an object for
`bios`, `()` for actions, `None`/a value for `Option`, etc.).

- `return Ok(v)` / a bare `v` means the method succeeds with `v`.
- `return Err(msg)`, or `?` on a failed call, means the method fails with a
  `RedfishError` carrying `msg`.

## The `ctx` handle

`ctx` is the BMC handle passed to every override. Its methods need BMC state, so
they live on `ctx`.

| Call | Returns | Notes |
|------|---------|-------|
| `ctx.get(path).await` | `Ok(#{status, headers, body})` / `Err(msg)` | `path` is relative to `redfish/v1/`, no leading `/` |
| `ctx.post(path, body).await` | `Ok(#{...})` / `Err(msg)` | `body` is any value (encoded as JSON) |
| `ctx.patch(path, body).await` | `Ok(#{...})` / `Err(msg)` | |
| `ctx.delete(path).await` | `Ok(#{...})` / `Err(msg)` | |
| `ctx.expand_collection(path).await` | `Ok(#{...})` / `Err(msg)` | Collection with `Members` inlined, tries `$expand` then GETs each member |
| `ctx.system_id()` | `String` | first system id resolved at client creation |
| `ctx.manager_id()` | `String` | first manager id |
| `ctx.variant()` | `Option<String>` | the override file's `variant`, if any |
| `ctx.vendor_data()` | `Option<value>` | the override file's `data`, if any |
| `ctx.bmc_address()` | `String` | BMC host/IP (the override file's `addr` key) |

The response object is `#{ status: <int>, headers: #{..}, body: <json or ()> }`.
Header names are lowercased.

JSON integers (in a response `body`, or passed to `json_decode`) are assumed to
fit in a signed 64-bit range. Rune has no unsigned integer type, so a JSON
integer above `i64::MAX` silently becomes negative rather than erroring.

## Free functions (called directly, no `ctx`)

These are pure/host helpers, so they are plain functions. Call them by name.

| Call | Returns | Notes |
|------|---------|-------|
| `sha256(text)` | `String` | lowercase-hex SHA-256 of the UTF-8 bytes |
| `sha512(text)` | `String` | lowercase-hex SHA-512 |
| `b64_encode(text)` | `String` | standard base64, padded |
| `b64_decode(text)` | `Ok(text)` / `Err(msg)` | errors on bad base64 or non-UTF-8 |
| `json_encode(value)` | `Ok(text)` / `Err(msg)` | serialize any value to JSON text |
| `json_decode(text)` | `Ok(value)` / `Err(msg)` | parse JSON text to a value |
| `read_file(path)` | `Ok(text)` / `Err(msg)` | read a file as UTF-8 (host privileges) |
| `read_env(name)` | `Option<String>` | env var value, or `None` if unset |
| `unix_time()` | `i64` | wall-clock seconds since the Unix epoch |

The ones returning `Ok/Err` can be matched or `?`-ed exactly like the HTTP verbs.

## Methods a script may override

Define a function with one of these names to take over that method. Arguments
shown are what the script receives (always `ctx` first).

### No arguments, `pub async fn name(ctx)`

```text
get_accounts            get_software_inventories  get_tasks
get_power_state         get_service_root          get_systems
get_system              get_managers              get_manager
get_secure_boot         disable_secure_boot       enable_secure_boot
bmc_reset               bmc_reset_to_defaults     get_system_event_log
set_machine_password_policy  setup_serial_console clear_tpm
pcie_devices            bios                      reset_bios
pending                 clear_pending             get_chassis_all
get_manager_ethernet_interfaces  get_system_ethernet_interfaces
get_update_service      get_base_mac_address      is_ipmi_over_lan_enabled
enable_rshim_bmc        clear_nvram               get_nic_mode
enable_infinite_boot    is_infinite_boot_enabled  get_host_rshim
get_boss_controller     get_component_integrities set_utc_timezone
get_power_metrics       get_thermal_metrics       get_drives_metrics
get_boot_options        ac_powercycle_supported_by_power
```

`ac_powercycle_supported_by_power` is resolved once, alongside `system_id`/`manager_id`,
since its trait method is synchronous and can't itself await a script call. Until that
first resolution runs, or if the script doesn't define it, it reports the standard
implementation's answer (`false` unless a vendor overrides it). A script that fails
this one call isn't retried later, so keep the override a plain hardcoded value rather
than anything that can fail transiently (e.g. a network call).

### String arguments, `pub async fn name(ctx, arg1, ...)`

| Function | Args |
|----------|------|
| `delete_user` | `username` |
| `get_firmware` | `id` |
| `get_task` | `id` |
| `get_secure_boot_certificate` | `database_id, certificate_id` |
| `get_secure_boot_certificates` | `database_id` |
| `add_secure_boot_certificate` | `pem_cert, database_id` |
| `get_boot_option` | `option_id` |
| `get_network_device_functions` | `chassis_id` |
| `get_chassis` | `id` |
| `get_chassis_assembly` | `chassis_id` |
| `get_chassis_network_adapters` | `chassis_id` |
| `get_chassis_network_adapter` | `chassis_id, id` |
| `get_base_network_adapters` | `system_id` |
| `get_base_network_adapter` | `system_id, id` |
| `get_ports` | `chassis_id, network_adapter` |
| `get_port` | `chassis_id, network_adapter, id` |
| `get_manager_ethernet_interface` | `id` |
| `get_system_ethernet_interface` | `id` |
| `change_username` | `old_name, new_name` |
| `change_password` | `username, new_pass` |
| `change_password_by_id` | `account_id, new_pass` |
| `change_uefi_password` | `current_uefi_password, new_uefi_password` |
| `clear_uefi_password` | `current_uefi_password` |
| `get_job_state` | `job_id` |
| `get_firmware_for_component` | `component_integrity_id` |
| `get_component_ca_certificate` | `url` |
| `trigger_evidence_collection` | `url, nonce` |
| `get_evidence` | `url` |
| `decommission_storage_controller` | `controller_id` |
| `create_storage_volume` | `controller_id, volume_name` |
| `get_collection` | `id` is a resource path relative to `redfish/v1/`, e.g. `Chassis`; return the body as an object (`#{...}`), not the full `Collection` |
| `set_ntp_servers` | `servers` is a list (`Vec`) of NTP server address strings, not a single string |

### Enum/struct arguments marshaled as strings

| Function | Args (script side) |
|----------|--------------------|
| `power` | `action` is e.g. `"On"`, `"ForceOff"`, `"GracefulRestart"` |
| `boot_once` | `target` is `"Pxe"`, `"Hdd"`, or `"UefiHttp"` |
| `boot_first` | `target`, the same set |
| `set_boot_override` | `target, enabled, mode, uri` (`mode`/`uri` may be `None`) |

### Extra Rust-only arguments are dropped (script gets just `ctx`)

`machine_setup`, `is_bios_setup`, `set_boot_order_dpu_first`, `is_boot_order_setup`.

### Always delegate, cannot be overridden from a script

These take non-`Deserialize`/complex arguments or return non-`Deserialize` types,
so they always run the standard implementation:

```text
get_gpu_sensors        lockdown_status            serial_console_status
create_user            chassis_reset              get_bmc_event_log
machine_setup_status   lockdown                   change_boot_order
update_firmware        update_firmware_multipart  update_firmware_simple_update
set_bios               get_network_device_function get_resource
lockdown_bmc           enable_ipmi_over_lan       set_nic_mode
set_host_rshim         set_idrac_lockdown         set_host_privilege_level
```

## Language & standard library

Scripts are ordinary Rune (0.14). Language: `let`, `if`/`else`, `match`, `for`,
`while`, `loop`, closures (`|x| ...`), `async`/`await`, the `?` operator, ranges
(`a..b`), template strings (`` `text ${expr}` ``), object literals
(`#{ key: value }`), vectors (`[1, 2]`), and tuples.

The tables below are the script-callable standard library from
`Context::with_default_modules()`. Instance methods are `value.method(...)`; free
functions/constructors are bare (`min(a, b)`, `String::new()`); macros end in `!`.

**Globals & macros** (free functions and macros available everywhere)

- Free fns: `min(a,b)`, `max(a,b)`, `clone(x)`, `drop(x)`, `print(x)`, `println(x)`,
  `panic(msg)`, `range(a,b)`
- Macros: `format!`, `println!`, `print!`, `panic!`, `assert!`, `assert_eq!`,
  `stringify!`, `file!`, `line!`

**Iterators** (chain off `.iter()`, a range, or any iterable)

`map`, `filter`, `filter_map`, `flat_map`, `enumerate`, `chain`, `skip`, `take`,
`peekable`, `rev`, `fold`, `reduce`, `find`, `any`, `all`, `count`, `sum`,
`product`, `nth`, `next`, `collect::<Vec>()` / `collect::<VecDeque>()`

**String**: `len`, `is_empty`, `capacity`, `char_at`, `chars`, `bytes`, `lines`,
`get`, `contains`, `starts_with`, `ends_with`, `find`, `split`, `split_once`,
`split_str`, `trim`, `trim_end`, `replace`, `to_lowercase`, `to_uppercase`,
`push`, `push_str`, `clear`, `reserve`, `as_bytes`, `into_bytes`,
`is_char_boundary`, `parse::<i64>()`/`parse::<f64>()`/`parse::<char>()`; ctors
`String::new`/`with_capacity`/`from`/`from_utf8`.

**Vec / `[...]`**: `len`, `is_empty`, `capacity`, `push`, `pop`, `insert`,
`remove`, `get`, `clear`, `extend`, `resize`, `sort`, `sort_by`, `iter`; ctors
`Vec::new`/`with_capacity`; iterate `for x in v`.

**Object / `#{...}`**: `get`, `contains_key`, `remove`; index `obj["key"]`;
iterate `for (k, v) in obj`.

**Option**: `is_some`, `is_none`, `unwrap`, `unwrap_or`, `unwrap_or_else`,
`expect`, `map`, `and_then`, `ok_or`, `ok_or_else`, `take`, `transpose`, `iter`.

**Result**: `is_ok`, `is_err`, `ok`, `unwrap`, `unwrap_or`, `unwrap_or_else`,
`expect`, `map`, `and_then`; plus `?`.

**Numbers** (integer and float methods)

- int (i64/u64): `abs`, `signum`, `pow`, `min`, `max`, `to_float`, `to_string`,
  `parse`, `is_positive`, `is_negative`, `checked_add/sub/mul/div/rem`,
  `saturating_*`, `wrapping_*`
- f64: `abs`, `ceil`, `floor`, `round`, `sqrt`, `powi`, `powf`, `is_nan`,
  `is_finite`, `is_infinite`, `is_normal`, `is_subnormal`, `to::<i64>()`, `parse`
- operators `+ - * / %` and comparisons work via protocols

**char**: `is_alphabetic`, `is_alphanumeric`, `is_numeric`, `is_whitespace`,
`is_control`, `is_uppercase`, `is_lowercase`, `to_digit`, `to_i64`; ctor
`char::from_i64`.

**Tuple**: `len`, `is_empty`, `get`, `iter`.

**Bytes**: `len`, `is_empty`, `push`, `pop`, `insert`, `remove`, `first`, `last`,
`extend`, `extend_str`, `as_vec`, `into_vec`, `clear`, `capacity`, `reserve`;
ctors `Bytes::new`/`with_capacity`/`from_vec`.

**Collections** (under `std::collections`, needs a `use` or a full path)

- `HashMap`: `new`, `with_capacity`, `from_iter`, `insert`, `get`, `remove`,
  `contains_key`, `keys`, `values`, `iter`, `len`, `is_empty`, `clear`,
  `capacity`, `extend`
- `HashSet`: `new`, `with_capacity`, `from_iter`, `insert`, `remove`, `contains`,
  `union`, `intersection`, `difference`, `iter`, `len`, `is_empty`, `clear`,
  `capacity`, `extend`
- `VecDeque`: `new`, `with_capacity`, `from_iter`, `from::<Vec>`, `push_back`,
  `push_front`, `pop_back`, `pop_front`, `front`, `back`, `insert`, `remove`,
  `rotate_left`, `rotate_right`, `iter`, `len`, `reserve`, `extend`

**Async & misc**: `future::join(..)` to await several futures; `cmp::Ordering`
(`Less`/`Equal`/`Greater`); `mem::drop`.

**Boundary:** that is the whole of "pure computation." Rune's std has **no**
filesystem, network, process, clock, or environment access. A script reaches the
outside world only through the `ctx` methods and the free functions above, all
registered by libredfish.

See `http_stub.rn` in this directory for runnable HTTP examples, and `generic.rn`
for a complete worked vendor override. It implements twelve methods against a
limited BMC and is exercised end to end by the
`rune_example_script_overrides_bmc_methods` integration test, so it stays honest
as the dispatched surface changes.

`sushy.rn` is a second complete, ready-to-copy example: it targets vanilla
(unpatched) OpenStack `sushy-tools` as a script, overriding only the handful of
methods where that emulator's behavior diverges from standard Redfish. Exercised
end to end by the `rune_sushy_script_overrides_bmc_methods` integration test. See
`sushy.md` for a per-method cURL cheat-sheet of what each override plugs and why.

`hw.rn` is the real-hardware counterpart, a Dell iDRAC override covering the
largest surface of the three: Oem-only paths (jobs, lockdown, BOSS storage,
`SetupPassword`), UEFI HTTP boot as BIOS attributes read from the firmware's own
registry, and boot ordering that iDRAC rejects on the standard properties. Every
override is gated on the Manager advertising Dell under `Oem`, so pointing it at
another vendor refuses rather than writing Dell attributes elsewhere. Exercised by
the `rune_hw_script_reads_dell_oem_paths` and
`rune_hw_script_without_a_pinned_boot_nic` integration tests. See `hw.md` for the
per-method cURL cheat-sheet.

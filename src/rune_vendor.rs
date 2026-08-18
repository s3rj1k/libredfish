//! `rune` vendor, a scriptable BMC backend picked by the vendor override file. Methods
//! dispatch to same named script functions, else fall back. See tests/rune/README.md.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, RwLock};
use std::time::{Duration, SystemTime};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use reqwest::header::HeaderMap;
use reqwest::{Method, StatusCode};
use rune::runtime::{Args, Ref, RuntimeContext, Unit, Value, VmError, VmResult};
use rune::{
    Any, Context, ContextError, Diagnostics, FromValue, Module, Source, Sources, ToValue, Vm,
};
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256, Sha512};

use crate::model::account_service::ManagerAccount;
use crate::model::certificate::Certificate;
use crate::model::component_integrity::{CaCertificate, ComponentIntegrities, Evidence};
use crate::model::oem::nvidia_dpu::{HostPrivilegeLevel, NicMode};
use crate::model::power::Power;
use crate::model::secure_boot::SecureBoot;
use crate::model::sel::LogEntry;
use crate::model::sensor::GPUSensors;
use crate::model::service_root::ServiceRoot;
use crate::model::software_inventory::SoftwareInventory;
use crate::model::storage::Drives;
use crate::model::task::Task;
use crate::model::thermal::Thermal;
use crate::model::update_service::{ComponentType, TransferProtocolType, UpdateService};
use crate::model::{BootOption, ComputerSystem, Manager, ODataId};
use crate::network::{
    manager_id_from_system, system_ids_for_bios_probe, RedfishHttpClient, REDFISH_ENDPOINT,
};
use crate::standard::RedfishStandard;
use crate::{
    Assembly, BiosProfileType, BiosProfileVendor, Boot, BootOptions, BootOverride, Chassis,
    Collection, EnabledDisabled, EthernetInterface, JobState, MachineSetupStatus, NetworkAdapter,
    NetworkDeviceFunction, NetworkPort, PCIeDevice, PowerState, Redfish, RedfishError,
    RedfishFuture, Resource, RoleId, Status, SystemPowerControl,
};

// Host API exposed to scripts.

/// Context handed to a Rune script. Holds the BMC HTTP client plus resolved ids and variant.
#[derive(Any, Clone)]
pub(crate) struct RedfishCtx {
    client: RedfishHttpClient,
    system_id: String,
    manager_id: String,
    variant: Option<String>,
    data: Option<serde_json::Value>,
}

fn vm_err<T>(msg: String) -> VmResult<T> {
    VmResult::err(VmError::panic(msg))
}

/// `GET {path}` returns `Ok(#{ status, headers, body })` or `Err(message)`.
#[rune::function(instance)]
async fn get(ctx: Ref<RedfishCtx>, path: String) -> VmResult<Value> {
    http_call(&ctx, Method::GET, &path, None).await
}

/// `PATCH {path}` with JSON `body` returns `Ok(#{ status, headers, body })` or `Err(message)`.
#[rune::function(instance)]
async fn patch(ctx: Ref<RedfishCtx>, path: String, body: Value) -> VmResult<Value> {
    http_call(&ctx, Method::PATCH, &path, Some(body)).await
}

/// `POST {path}` with JSON `body` returns `Ok(#{ status, headers, body })` or `Err(message)`.
#[rune::function(instance)]
async fn post(ctx: Ref<RedfishCtx>, path: String, body: Value) -> VmResult<Value> {
    http_call(&ctx, Method::POST, &path, Some(body)).await
}

/// `DELETE {path}` returns `Ok(#{ status, headers, body })` or `Err(message)`.
#[rune::function(instance)]
async fn delete(ctx: Ref<RedfishCtx>, path: String) -> VmResult<Value> {
    http_call(&ctx, Method::DELETE, &path, None).await
}

/// `expand_collection(path)` GETs the Collection at `path` with `Members` inlined.
/// Tries server side `$expand` first, then fetches each member individually.
#[rune::function(instance)]
async fn expand_collection(ctx: Ref<RedfishCtx>, path: String) -> VmResult<Value> {
    result_to_value(do_expand_collection(&ctx, &path).await, "expand_collection")
}

async fn do_expand_collection(ctx: &RedfishCtx, path: &str) -> Result<Value, String> {
    // Ask the server to expand first, one request when the BMC honors $expand. A BMC
    // that ignores it just returns Members unexpanded, detected below.
    let sep = if path.contains('?') { "&" } else { "?" };
    let expand_path = format!("{path}{sep}$expand=.($levels=1)");
    let (_, body, _) = ctx
        .client
        .req::<serde_json::Value, serde_json::Value>(
            Method::GET,
            &expand_path,
            None,
            None,
            None,
            Vec::new(),
        )
        .await
        .map_err(|e| format!("GET {expand_path}: {e}"))?;
    let mut body = body.ok_or_else(|| format!("GET {expand_path}: empty body"))?;

    let members = body
        .get("Members")
        .and_then(|m| m.as_array())
        .cloned()
        .unwrap_or_default();

    // A shallow (unexpanded) member has only `@odata.*` keys. If every member
    // already has more than that, the server did the expansion for us.
    let already_expanded = members.iter().all(|m| {
        m.as_object()
            .is_some_and(|o| o.keys().any(|k| !k.starts_with("@odata.")))
    });
    if already_expanded {
        return serde_json::from_value(body).map_err(|e| format!("GET {expand_path}: decode: {e}"));
    }

    let mut expanded = Vec::with_capacity(members.len());
    for member in members {
        let odata_id = member
            .get("@odata.id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("GET {path}: Members entry missing @odata.id"))?;
        let ref_path = odata_id.replace(&format!("/{REDFISH_ENDPOINT}/"), "");
        let (_, sub_body, _) = ctx
            .client
            .req::<serde_json::Value, serde_json::Value>(
                Method::GET,
                &ref_path,
                None,
                None,
                None,
                Vec::new(),
            )
            .await
            .map_err(|e| format!("GET {ref_path}: {e}"))?;
        expanded.push(sub_body.ok_or_else(|| format!("GET {ref_path}: empty body"))?);
    }
    if let Some(obj) = body.as_object_mut() {
        obj.insert("Members".to_string(), serde_json::Value::Array(expanded));
    }
    serde_json::from_value(body).map_err(|e| format!("GET {path}: decode: {e}"))
}

/// Run an HTTP request and hand the script a `Result` value (built via `ToValue`) so
/// scripts can `match` or `?` it instead of the VM unwinding.
async fn http_call(
    ctx: &RedfishCtx,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> VmResult<Value> {
    result_to_value(do_http(ctx, method, path, body).await, "http")
}

/// The request itself. Returns `Ok(#{status, headers, body})` on a completed HTTP
/// exchange and `Err(message)` on a transport or encode failure.
async fn do_http(
    ctx: &RedfishCtx,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> Result<Value, String> {
    let json_body: Option<serde_json::Value> = match body {
        Some(v) => Some(
            serde_json::to_value(&v).map_err(|e| format!("{method} {path}: encode body: {e}"))?,
        ),
        None => None,
    };
    match ctx
        .client
        .req::<serde_json::Value, serde_json::Value>(
            method.clone(),
            path,
            json_body,
            None,
            None,
            Vec::new(),
        )
        .await
    {
        Ok((status, body_opt, headers_opt)) => {
            let resp = response_json(status, headers_opt, body_opt);
            serde_json::from_value::<Value>(resp)
                .map_err(|e| format!("{method} {path}: decode response: {e}"))
        }
        Err(e) => Err(format!("{method} {path}: {e}")),
    }
}

/// Build the `#{ status, headers, body }` response object scripts receive.
fn response_json(
    status: StatusCode,
    headers: Option<HeaderMap>,
    body: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut hdrs = serde_json::Map::new();
    if let Some(h) = headers {
        for (name, value) in &h {
            let value = value.to_str().unwrap_or_default();
            // Repeated header lines are comma joined per RFC 7230, so a duplicate name does
            // not silently overwrite the first.
            hdrs.entry(name.as_str().to_string())
                .and_modify(|existing| {
                    if let serde_json::Value::String(s) = existing {
                        s.push_str(", ");
                        s.push_str(value);
                    }
                })
                .or_insert_with(|| serde_json::Value::String(value.to_string()));
        }
    }
    serde_json::json!({
        "status": status.as_u16(),
        "headers": serde_json::Value::Object(hdrs),
        "body": body.unwrap_or(serde_json::Value::Null),
    })
}

/// Bridge a rune `Value` to `T` via serde_json.
fn bridge<T: DeserializeOwned>(value: &Value, name: &str) -> Result<T, RedfishError> {
    let json = serde_json::to_value(value).map_err(|e| RedfishError::GenericError {
        error: format!("rune {name}: result encode: {e}"),
    })?;
    serde_json::from_value::<T>(json).map_err(|e| RedfishError::GenericError {
        error: format!("rune {name}: result -> {}: {e}", std::any::type_name::<T>()),
    })
}

/// Stringify the payload `Value` of a script `Err`.
fn value_to_string(v: &Value) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => s,
        Ok(other) => other.to_string(),
        Err(_) => "<unprintable rune error>".to_string(),
    }
}

/// Interpret a script's return value. A top level `Err(..)` becomes a [`RedfishError`],
/// an `Ok(v)` is unwrapped, anything else bridges directly.
fn interpret<T: DeserializeOwned>(value: &Value, name: &str) -> Result<T, RedfishError> {
    // `from_value` consumes its argument, so hand it a clone (cheap, a rune
    // `Value` is a refcounted handle) and keep `value` for when it isn't a `Result`.
    match <Result<Value, Value>>::from_value(value.clone()) {
        Ok(Ok(inner)) => bridge(&inner, name),
        Ok(Err(e)) => Err(RedfishError::GenericError {
            error: format!("rune {name}: script error: {}", value_to_string(&e)),
        }),
        Err(_) => bridge(value, name),
    }
}

#[rune::function(instance)]
fn system_id(ctx: &RedfishCtx) -> String {
    ctx.system_id.clone()
}

#[rune::function(instance)]
fn manager_id(ctx: &RedfishCtx) -> String {
    ctx.manager_id.clone()
}

#[rune::function(instance)]
fn variant(ctx: &RedfishCtx) -> Option<String> {
    ctx.variant.clone()
}

/// `ctx.vendor_data()` returns the free form JSON blob from the override file's `data`
/// field, or `None`. A script shapes and looks up whatever it needs from it.
#[rune::function(instance)]
fn vendor_data(ctx: &RedfishCtx) -> VmResult<Option<Value>> {
    let Some(data) = &ctx.data else {
        return VmResult::Ok(None);
    };
    match serde_json::from_value::<Value>(data.clone()) {
        Ok(v) => VmResult::Ok(Some(v)),
        Err(e) => vm_err(format!("vendor_data: decode: {e}")),
    }
}

/// `ctx.bmc_address()` returns the BMC host this client targets, no scheme or port.
/// Same address the override file matched on, so a script can key behavior per host.
#[rune::function(instance)]
fn bmc_address(ctx: &RedfishCtx) -> String {
    ctx.client.host().to_string()
}

/// `sha256(data)` returns lowercase hex sha256 of `data`'s UTF8 bytes. Free function.
#[rune::function]
fn sha256(data: String) -> String {
    hex_lower(Sha256::digest(data.as_bytes()))
}

/// `sha512(data)` returns lowercase hex sha512 of `data`'s UTF8 bytes. Free function.
#[rune::function]
fn sha512(data: String) -> String {
    hex_lower(Sha512::digest(data.as_bytes()))
}

/// Lowercase hex encode bytes (backs the `sha256`/`sha512` script helpers).
fn hex_lower(bytes: impl AsRef<[u8]>) -> String {
    use std::fmt::Write as _;
    let bytes = bytes.as_ref();
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Hand a `Result<T, String>` to a script as a matchable rune `Ok(..)`/`Err(..)` value (the
/// same convention the HTTP verbs use), or raise a VM error if the value can't be encoded.
fn result_to_value<T: ToValue>(result: Result<T, String>, name: &str) -> VmResult<Value> {
    match result.to_value() {
        Ok(v) => VmResult::Ok(v),
        Err(e) => vm_err(format!("rune {name}: encode result: {e}")),
    }
}

/// `b64_encode(data)` returns padded, standard alphabet base64 of `data`'s UTF8 bytes.
/// Free function.
#[rune::function]
fn b64_encode(data: String) -> String {
    BASE64.encode(data.as_bytes())
}

/// `b64_decode(data)` returns `Ok(text)` for valid standard base64 decoding to UTF8,
/// else `Err(message)`. Match it or `?` it like the HTTP verbs. Free function.
#[rune::function]
fn b64_decode(data: String) -> VmResult<Value> {
    result_to_value(do_b64_decode(&data), "b64_decode")
}

fn do_b64_decode(data: &str) -> Result<String, String> {
    let bytes = BASE64
        .decode(data.as_bytes())
        .map_err(|e| format!("b64_decode: {e}"))?;
    String::from_utf8(bytes).map_err(|e| format!("b64_decode: invalid utf-8: {e}"))
}

/// `json_encode(value)` returns `Ok(json_text)` for any serializable value, else `Err(message)`.
/// Free function.
#[rune::function]
fn json_encode(value: Value) -> VmResult<Value> {
    let encoded = serde_json::to_string(&value).map_err(|e| format!("json_encode: {e}"));
    result_to_value(encoded, "json_encode")
}

/// `json_decode(text)` returns `Ok(value)` for valid JSON (object/array/scalar), else `Err(message)`.
/// Free function.
#[rune::function]
fn json_decode(data: String) -> VmResult<Value> {
    let decoded = serde_json::from_str::<Value>(&data).map_err(|e| format!("json_decode: {e}"));
    result_to_value(decoded, "json_decode")
}

/// `read_file(path)` returns `Ok(contents)` reading `path` as UTF8 text, else
/// `Err(message)`. Reaches the host filesystem, trusted scripts only. Free function.
#[rune::function]
fn read_file(path: String) -> VmResult<Value> {
    let read = std::fs::read_to_string(&path).map_err(|e| format!("read_file {path}: {e}"));
    result_to_value(read, "read_file")
}

/// `read_env(name)` returns the value of environment variable `name`, or `None` when
/// unset or not UTF8. Reads the process environment, trusted scripts only.
#[rune::function]
fn read_env(name: String) -> Option<String> {
    std::env::var(name).ok()
}

/// `unix_time()` returns current wall clock Unix time in whole seconds since the epoch (0 if the
/// clock is set before 1970). Free function.
#[rune::function]
fn unix_time() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// The libredfish host module registered into the Rune context.
fn module() -> Result<Module, ContextError> {
    let mut m = Module::new();
    m.ty::<RedfishCtx>()?;
    m.function_meta(get)?;
    m.function_meta(patch)?;
    m.function_meta(post)?;
    m.function_meta(delete)?;
    m.function_meta(expand_collection)?;
    m.function_meta(system_id)?;
    m.function_meta(manager_id)?;
    m.function_meta(variant)?;
    m.function_meta(vendor_data)?;
    m.function_meta(bmc_address)?;
    m.function_meta(sha256)?;
    m.function_meta(sha512)?;
    m.function_meta(b64_encode)?;
    m.function_meta(b64_decode)?;
    m.function_meta(json_encode)?;
    m.function_meta(json_decode)?;
    m.function_meta(read_file)?;
    m.function_meta(read_env)?;
    m.function_meta(unix_time)?;
    Ok(m)
}

// Compilation / runtime.

fn ctx_err(e: impl std::fmt::Display) -> RedfishError {
    RedfishError::GenericError {
        error: format!("rune context: {e}"),
    }
}

/// Build a compile and runtime context from the default std modules plus our host module.
fn build_context() -> Result<Context, RedfishError> {
    let mut context = Context::with_default_modules().map_err(ctx_err)?;
    context
        .install(module().map_err(ctx_err)?)
        .map_err(ctx_err)?;
    Ok(context)
}

/// Shared runtime context, built once from the same module set the units compile against.
fn shared_runtime() -> Result<Arc<RuntimeContext>, RedfishError> {
    static RT: OnceLock<Arc<RuntimeContext>> = OnceLock::new();
    if let Some(rt) = RT.get() {
        return Ok(rt.clone());
    }
    let context = build_context()?;
    let rt = Arc::new(context.runtime().map_err(ctx_err)?);
    let _ = RT.set(rt.clone());
    Ok(rt)
}

/// Cache of compiled units, keyed by script path (invalidated by mtime).
type UnitCache = HashMap<PathBuf, (SystemTime, Arc<Unit>)>;

/// Compile a script to a `Unit`, cached by path + mtime (recompiled on change).
fn compile(path: &str) -> Result<Arc<Unit>, RedfishError> {
    static CACHE: OnceLock<Mutex<UnitCache>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let p = PathBuf::from(path);

    let mtime = std::fs::metadata(&p)
        .and_then(|m| m.modified())
        .map_err(|e| RedfishError::FileError(format!("rune script {path}: {e}")))?;
    // A poisoned cache is harmless, entries are immutable compiled units, so
    // recover the guard rather than propagating another thread's panic.
    if let Some((t, u)) = cache.lock().unwrap_or_else(PoisonError::into_inner).get(&p) {
        if *t == mtime {
            return Ok(u.clone());
        }
    }

    let src = std::fs::read_to_string(&p)
        .map_err(|e| RedfishError::FileError(format!("rune script {path}: {e}")))?;
    let context = build_context()?;
    let mut sources = Sources::new();
    sources
        .insert(
            Source::new(path, src)
                .map_err(|e| RedfishError::FileError(format!("rune source {path}: {e}")))?,
        )
        .map_err(|e| RedfishError::FileError(format!("rune sources {path}: {e}")))?;
    let mut diagnostics = Diagnostics::new();
    let unit = rune::prepare(&mut sources)
        .with_context(&context)
        .with_diagnostics(&mut diagnostics)
        .build()
        .map_err(|e| RedfishError::FileError(format!("rune compile {path}: {e}")))?;

    let unit = Arc::new(unit);
    cache
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(p, (mtime, unit.clone()));
    Ok(unit)
}

// The vendor.

/// Trace every dispatched call and whether script or fallback served it, at debug
/// level on the rune_vendor target. Values stay out, some methods carry secrets.
macro_rules! trace_call {
    ($method:expr, $via:expr) => {
        tracing::debug!(target: "rune_vendor", method = $method, via = $via, "rune vendor call");
    };
}

pub(crate) struct Bmc {
    /// The wrapped standard client. Locked so `resolve_ids` can update
    /// `system_id`/`manager_id` from a `&self` dispatch.
    s: RwLock<RedfishStandard>,
    unit: Arc<Unit>,
    runtime: Arc<RuntimeContext>,
    /// Runs `resolve_ids` exactly once, even under concurrent dispatch.
    resolved: tokio::sync::OnceCell<()>,
    /// Cached script answer for `ac_powercycle_supported_by_power`, computed in
    /// `resolve_ids` because that trait method is sync. Unset means fall back.
    ac_powercycle_supported: OnceLock<bool>,
}

impl Bmc {
    pub(crate) fn new(s: RedfishStandard) -> Result<Self, RedfishError> {
        let path = s.vendor_script().ok_or_else(|| {
            RedfishError::FileError(
                "Rune vendor selected but no script set (override entry needs a \"script\" path)"
                    .to_string(),
            )
        })?;
        let unit = compile(path)?;
        let runtime = shared_runtime()?;
        Ok(Self {
            s: RwLock::new(s),
            unit,
            runtime,
            resolved: tokio::sync::OnceCell::new(),
            ac_powercycle_supported: OnceLock::new(),
        })
    }

    /// Read lock, clone (cheap), and hand back the wrapped standard client.
    fn snapshot(&self) -> RedfishStandard {
        self.s
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Resolve ids and service root if unset, the way `RedfishClientPool` does for
    /// every other vendor, so a directly built client targets the same host.
    async fn resolve_ids(&self) -> Result<(), RedfishError> {
        let mut s = self.snapshot();
        if s.system_id().is_empty() || s.manager_id().is_empty() {
            // Only a manager id we resolved ourselves may be moved by the Bios probe
            // below. One the caller set explicitly stands.
            let manager_unset = s.manager_id().is_empty();
            let mut manager_id = if manager_unset {
                let managers = s.get_managers().await?;
                managers
                    .first()
                    .ok_or_else(|| RedfishError::GenericError {
                        error: "No managers found in service root".to_string(),
                    })?
                    .clone()
            } else {
                s.manager_id().to_string()
            };

            if s.system_id().is_empty() {
                let systems = s.get_systems().await?;
                let preferred_system_id = systems
                    .iter()
                    .find(|id| *id == "System_0")
                    .or_else(|| systems.first())
                    .ok_or_else(|| RedfishError::GenericError {
                        error: "No systems found in service root".to_string(),
                    })?;

                // Treat any fetch error as "no Bios here", same as the pool. The
                // preferred id is already the fallback.
                let mut system_with_bios: Option<ComputerSystem> = None;
                for system_member in system_ids_for_bios_probe(preferred_system_id, &systems) {
                    system_with_bios = s.if_system_has_bios(system_member).await;
                    if system_with_bios.is_some() {
                        break;
                    }
                }

                if manager_unset {
                    manager_id = system_with_bios
                        .as_ref()
                        .and_then(manager_id_from_system)
                        .unwrap_or(manager_id);
                }

                let system_id = system_with_bios
                    .map(|swb| swb.id.to_owned())
                    .unwrap_or_else(|| preferred_system_id.to_owned());
                s.set_system_id(&system_id)?;
            }

            // Set the settled manager id before get_service_root(), so its vendor
            // override lookup is keyed by the manager this client actually uses.
            s.set_manager_id(&manager_id)?;
            let service_root = s.get_service_root().await?;
            s.set_service_root(service_root)?;
            *self.s.write().unwrap_or_else(PoisonError::into_inner) = s.clone();
        }
        // ac_powercycle_supported_by_power is sync and cannot await a script call, so
        // resolve it here once alongside the rest of setup and cache it.
        if self.has("ac_powercycle_supported_by_power") {
            let ctx = RedfishCtx {
                client: s.client.clone(),
                system_id: s.system_id().to_string(),
                manager_id: s.manager_id().to_string(),
                variant: s.vendor_variant().map(str::to_string),
                data: s.vendor_data().cloned(),
            };
            if let Ok(supported) = self
                .call::<_, bool>("ac_powercycle_supported_by_power", (ctx,))
                .await
            {
                let _ = self.ac_powercycle_supported.set(supported);
            }
        }
        Ok(())
    }

    /// The wrapped standard client, guaranteed to have ids resolved (see `resolve_ids`).
    async fn resolved(&self) -> Result<RedfishStandard, RedfishError> {
        self.resolved.get_or_try_init(|| self.resolve_ids()).await?;
        Ok(self.snapshot())
    }

    /// Build a fresh script context, resolving ids on first use (see `resolved`).
    async fn ctx(&self) -> Result<RedfishCtx, RedfishError> {
        let s = self.resolved().await?;
        Ok(RedfishCtx {
            client: s.client.clone(),
            system_id: s.system_id().to_string(),
            manager_id: s.manager_id().to_string(),
            variant: s.vendor_variant().map(str::to_string),
            data: s.vendor_data().cloned(),
        })
    }

    /// True if the script defines a top level function `name`.
    fn has(&self, name: &str) -> bool {
        Vm::new(self.runtime.clone(), self.unit.clone())
            .lookup_function([name])
            .is_ok()
    }

    /// Call script function `name` with `args` (Send native values), deserialize the result.
    async fn call<A, T>(&self, name: &str, args: A) -> Result<T, RedfishError>
    where
        A: Args + Send,
        T: DeserializeOwned,
    {
        let execution = Vm::new(self.runtime.clone(), self.unit.clone())
            .send_execute([name], args)
            .map_err(|e| RedfishError::GenericError {
                error: format!("rune {name}: {e}"),
            })?;
        let value = execution
            .async_complete()
            .await
            .into_result()
            .map_err(|e| RedfishError::GenericError {
                error: format!("rune {name}: {e}"),
            })?;
        interpret::<T>(&value, name)
    }
}

// Per method dispatch generators, script if defined else `self.s`. The macros own
// the lifetime. Entries end with a semicolon so return types can contain commas.
macro_rules! dispatch_noarg {
    ($($name:ident -> $ret:ty);* $(;)?) => {$(
        fn $name<'a>(&'a self) -> RedfishFuture<'a, Result<$ret, RedfishError>> {
            let scripted = self.has(stringify!($name));
            trace_call!(stringify!($name), if scripted { "script" } else { "standard" });
            if scripted {
                Box::pin(async move {
                    let ctx = self.ctx().await?;
                    self.call::<_, $ret>(stringify!($name), (ctx,)).await
                })
            } else {
                Box::pin(async move { self.resolved().await?.$name().await })
            }
        }
    )*};
}

macro_rules! dispatch_noarg_boxed {
    ($($name:ident -> $ret:ty);* $(;)?) => {$(
        fn $name<'a>(&'a self) -> RedfishFuture<'a, Result<$ret, RedfishError>> {
            let scripted = self.has(stringify!($name));
            trace_call!(stringify!($name), if scripted { "script" } else { "standard" });
            if scripted {
                Box::pin(async move {
                    let ctx = self.ctx().await?;
                    self.call::<_, $ret>(stringify!($name), (ctx,)).await
                })
            } else {
                Box::pin(async move { self.resolved().await?.$name().await })
            }
        }
    )*};
}

macro_rules! dispatch_str {
    ($($name:ident ( $($arg:ident),+ ) -> $ret:ty);* $(;)?) => {$(
        fn $name<'a>(&'a self $(, $arg: &'a str)+) -> RedfishFuture<'a, Result<$ret, RedfishError>> {
            let scripted = self.has(stringify!($name));
            trace_call!(stringify!($name), if scripted { "script" } else { "standard" });
            if scripted {
                Box::pin(async move {
                    let ctx = self.ctx().await?;
                    self.call::<_, $ret>(stringify!($name), (ctx, $($arg.to_string()),+)).await
                })
            } else {
                Box::pin(async move { self.resolved().await?.$name($($arg),+).await })
            }
        }
    )*};
}

// Args that are vendor enums reach the script as their Redfish spelling, which is the
// same string the standard implementations put on the wire.
macro_rules! dispatch_display {
    ($($name:ident ( $($arg:ident : $ty:ty),+ ) -> $ret:ty);* $(;)?) => {$(
        fn $name<'a>(&'a self $(, $arg: $ty)+) -> RedfishFuture<'a, Result<$ret, RedfishError>> {
            let scripted = self.has(stringify!($name));
            trace_call!(stringify!($name), if scripted { "script" } else { "standard" });
            if scripted {
                $(let $arg = $arg;)+
                Box::pin(async move {
                    let ctx = self.ctx().await?;
                    self.call::<_, $ret>(stringify!($name), (ctx, $($arg.to_string()),+)).await
                })
            } else {
                Box::pin(async move { self.resolved().await?.$name($($arg),+).await })
            }
        }
    )*};
}

/// The Redfish `BootSourceOverrideTarget` string for a `Boot` value.
const fn boot_target_str(target: Boot) -> &'static str {
    match target {
        Boot::Pxe => "Pxe",
        Boot::HardDisk => "Hdd",
        Boot::UefiHttp => "UefiHttp",
    }
}

impl Redfish for Bmc {
    dispatch_noarg! {
        get_accounts -> Vec<ManagerAccount>;
        get_software_inventories -> Vec<String>;
        get_tasks -> Vec<String>;
        get_power_state -> PowerState;
        get_service_root -> ServiceRoot;
        get_systems -> Vec<String>;
        get_system -> ComputerSystem;
        get_managers -> Vec<String>;
        get_manager -> Manager;
        get_secure_boot -> SecureBoot;
        disable_secure_boot -> ();
        enable_secure_boot -> ();
        bmc_reset -> ();
        bmc_reset_to_defaults -> ();
        get_system_event_log -> Vec<LogEntry>;
        set_machine_password_policy -> ();
        setup_serial_console -> ();
        clear_tpm -> ();
        pcie_devices -> Vec<PCIeDevice>;
        bios -> HashMap<String, serde_json::Value>;
        reset_bios -> ();
        pending -> HashMap<String, serde_json::Value>;
        clear_pending -> ();
        get_chassis_all -> Vec<String>;
        get_manager_ethernet_interfaces -> Vec<String>;
        get_system_ethernet_interfaces -> Vec<String>;
        get_update_service -> UpdateService;
        get_base_mac_address -> Option<String>;
        is_ipmi_over_lan_enabled -> bool;
        enable_rshim_bmc -> ();
        clear_nvram -> ();
        get_nic_mode -> Option<NicMode>;
        enable_infinite_boot -> ();
        is_infinite_boot_enabled -> Option<bool>;
        get_host_rshim -> Option<EnabledDisabled>;
        get_boss_controller -> Option<String>;
        get_component_integrities -> ComponentIntegrities;
        set_utc_timezone -> ();
        get_gpu_sensors -> Vec<GPUSensors>;
        lockdown_status -> Status;
        serial_console_status -> Status;
    }

    dispatch_noarg_boxed! {
        get_power_metrics -> Power;
        get_thermal_metrics -> Thermal;
        get_drives_metrics -> Vec<Drives>;
        get_boot_options -> BootOptions;
    }

    dispatch_str! {
        delete_user(username) -> ();
        get_firmware(id) -> SoftwareInventory;
        get_task(id) -> Task;
        get_secure_boot_certificate(database_id, certificate_id) -> Certificate;
        get_secure_boot_certificates(database_id) -> Vec<String>;
        add_secure_boot_certificate(pem_cert, database_id) -> Task;
        get_boot_option(option_id) -> BootOption;
        get_network_device_functions(chassis_id) -> Vec<String>;
        get_chassis(id) -> Chassis;
        get_chassis_assembly(chassis_id) -> Assembly;
        get_chassis_network_adapters(chassis_id) -> Vec<String>;
        get_chassis_network_adapter(chassis_id, id) -> NetworkAdapter;
        get_base_network_adapters(system_id) -> Vec<String>;
        get_base_network_adapter(system_id, id) -> NetworkAdapter;
        get_ports(chassis_id, network_adapter) -> Vec<String>;
        get_port(chassis_id, network_adapter, id) -> NetworkPort;
        get_manager_ethernet_interface(id) -> EthernetInterface;
        get_system_ethernet_interface(id) -> EthernetInterface;
        change_username(old_name, new_name) -> ();
        change_password(username, new_pass) -> ();
        change_password_by_id(account_id, new_pass) -> ();
        change_uefi_password(current_uefi_password, new_uefi_password) -> Option<String>;
        clear_uefi_password(current_uefi_password) -> Option<String>;
        get_job_state(job_id) -> JobState;
        get_firmware_for_component(component_integrity_id) -> SoftwareInventory;
        get_component_ca_certificate(url) -> CaCertificate;
        trigger_evidence_collection(url, nonce) -> Task;
        get_evidence(url) -> Evidence;
        decommission_storage_controller(controller_id) -> Option<String>;
        create_storage_volume(controller_id, volume_name) -> Option<String>;
    }

    // Dispatched (enum arg marshaled as a string).
    fn power<'a>(
        &'a self,
        action: SystemPowerControl,
    ) -> RedfishFuture<'a, Result<(), RedfishError>> {
        let scripted = self.has("power");
        trace_call!("power", if scripted { "script" } else { "standard" });
        if scripted {
            Box::pin(async move {
                let ctx = self.ctx().await?;
                self.call::<_, ()>("power", (ctx, action.to_string())).await
            })
        } else {
            Box::pin(async move { self.resolved().await?.power(action).await })
        }
    }

    // Return types without `Deserialize` can't use the script return bridge, so delegate.
    dispatch_display! {
        create_user(username: &'a str, password: &'a str, role_id: RoleId) -> ();
        chassis_reset(chassis_id: &'a str, reset_type: SystemPowerControl) -> ();
    }

    // The cutoff reaches the script as an RFC 3339 string, or unset when there is none.
    fn get_bmc_event_log<'a>(
        &'a self,
        from: Option<chrono::DateTime<chrono::Utc>>,
    ) -> RedfishFuture<'a, Result<Vec<LogEntry>, RedfishError>> {
        let scripted = self.has("get_bmc_event_log");
        trace_call!(
            "get_bmc_event_log",
            if scripted { "script" } else { "standard" }
        );
        if scripted {
            let since = from.map(|d| d.to_rfc3339());
            Box::pin(async move {
                let ctx = self.ctx().await?;
                self.call::<_, Vec<LogEntry>>("get_bmc_event_log", (ctx, since))
                    .await
            })
        } else {
            Box::pin(async move { self.resolved().await?.get_bmc_event_log(from).await })
        }
    }

    fn machine_setup<'a>(
        &'a self,
        boot_interface: Option<crate::BootInterfaceRef<'a>>,
        bios_profiles: &'a BiosProfileVendor,
        selected_profile: BiosProfileType,
        oem_manager_profiles: &'a BiosProfileVendor,
    ) -> RedfishFuture<'a, Result<Option<String>, RedfishError>> {
        let scripted = self.has("machine_setup");
        trace_call!(
            "machine_setup",
            if scripted { "script" } else { "standard" }
        );
        if scripted {
            Box::pin(async move {
                let ctx = self.ctx().await?;
                self.call::<_, Option<String>>("machine_setup", (ctx,))
                    .await
            })
        } else {
            Box::pin(async move {
                self.resolved()
                    .await?
                    .machine_setup(
                        boot_interface,
                        bios_profiles,
                        selected_profile,
                        oem_manager_profiles,
                    )
                    .await
            })
        }
    }

    // The boot interface stays behind, as it does for is_boot_order_setup. A script that
    // needs it reads the same thing back off the BMC.
    fn machine_setup_status<'a>(
        &'a self,
        boot_interface: Option<crate::BootInterfaceRef<'a>>,
    ) -> RedfishFuture<'a, Result<MachineSetupStatus, RedfishError>> {
        let scripted = self.has("machine_setup_status");
        trace_call!(
            "machine_setup_status",
            if scripted { "script" } else { "standard" }
        );
        if scripted {
            Box::pin(async move {
                let ctx = self.ctx().await?;
                self.call::<_, MachineSetupStatus>("machine_setup_status", (ctx,))
                    .await
            })
        } else {
            Box::pin(async move {
                self.resolved()
                    .await?
                    .machine_setup_status(boot_interface)
                    .await
            })
        }
    }

    fn is_bios_setup<'a>(
        &'a self,
        boot_interface: Option<crate::BootInterfaceRef<'a>>,
    ) -> RedfishFuture<'a, Result<bool, RedfishError>> {
        let scripted = self.has("is_bios_setup");
        trace_call!(
            "is_bios_setup",
            if scripted { "script" } else { "standard" }
        );
        if scripted {
            Box::pin(async move {
                let ctx = self.ctx().await?;
                self.call::<_, bool>("is_bios_setup", (ctx,)).await
            })
        } else {
            Box::pin(async move { self.resolved().await?.is_bios_setup(boot_interface).await })
        }
    }

    dispatch_display! {
        lockdown(target: EnabledDisabled) -> ();
    }

    fn boot_once<'a>(&'a self, target: Boot) -> RedfishFuture<'a, Result<(), RedfishError>> {
        let scripted = self.has("boot_once");
        trace_call!("boot_once", if scripted { "script" } else { "standard" });
        if scripted {
            let t = boot_target_str(target).to_string();
            Box::pin(async move {
                let ctx = self.ctx().await?;
                self.call::<_, ()>("boot_once", (ctx, t)).await
            })
        } else {
            Box::pin(async move { self.resolved().await?.boot_once(target).await })
        }
    }

    fn boot_first<'a>(&'a self, target: Boot) -> RedfishFuture<'a, Result<(), RedfishError>> {
        let scripted = self.has("boot_first");
        trace_call!("boot_first", if scripted { "script" } else { "standard" });
        if scripted {
            let t = boot_target_str(target).to_string();
            Box::pin(async move {
                let ctx = self.ctx().await?;
                self.call::<_, ()>("boot_first", (ctx, t)).await
            })
        } else {
            Box::pin(async move { self.resolved().await?.boot_first(target).await })
        }
    }

    fn set_boot_override<'a>(
        &'a self,
        settings: BootOverride,
    ) -> RedfishFuture<'a, Result<Option<String>, RedfishError>> {
        let scripted = self.has("set_boot_override");
        trace_call!(
            "set_boot_override",
            if scripted { "script" } else { "standard" }
        );
        if scripted {
            let target = settings.target.to_string();
            let enabled = settings.enabled.to_string();
            let mode = settings.mode.as_ref().map(ToString::to_string);
            let uri = settings.http_boot_uri;
            Box::pin(async move {
                let ctx = self.ctx().await?;
                self.call::<_, Option<String>>(
                    "set_boot_override",
                    (ctx, target, enabled, mode, uri),
                )
                .await
            })
        } else {
            Box::pin(async move { self.resolved().await?.set_boot_override(settings).await })
        }
    }

    fn change_boot_order<'a>(
        &'a self,
        boot_array: Vec<String>,
    ) -> RedfishFuture<'a, Result<(), RedfishError>> {
        let scripted = self.has("change_boot_order");
        trace_call!(
            "change_boot_order",
            if scripted { "script" } else { "standard" }
        );
        if scripted {
            let order = boot_array.clone();
            Box::pin(async move {
                let ctx = self.ctx().await?;
                self.call::<_, ()>("change_boot_order", (ctx, order)).await
            })
        } else {
            Box::pin(async move { self.resolved().await?.change_boot_order(boot_array).await })
        }
    }

    fn set_ntp_servers<'a>(
        &'a self,
        servers: &'a [String],
    ) -> RedfishFuture<'a, Result<(), RedfishError>> {
        let scripted = self.has("set_ntp_servers");
        trace_call!(
            "set_ntp_servers",
            if scripted { "script" } else { "standard" }
        );
        if scripted {
            let servers = servers.to_vec();
            Box::pin(async move {
                let ctx = self.ctx().await?;
                self.call::<_, ()>("set_ntp_servers", (ctx, servers)).await
            })
        } else {
            Box::pin(async move { self.resolved().await?.set_ntp_servers(servers).await })
        }
    }

    // A file handle cannot cross into the vm, so the image is read here and handed over
    // base64 encoded. ctx.b64_decode turns it back into bytes. Whole image sits in memory.
    fn update_firmware<'a>(
        &'a self,
        filename: tokio::fs::File,
    ) -> RedfishFuture<'a, Result<Task, RedfishError>> {
        let scripted = self.has("update_firmware");
        trace_call!(
            "update_firmware",
            if scripted { "script" } else { "standard" }
        );
        if scripted {
            Box::pin(async move {
                let mut file = filename;
                let mut buf = Vec::new();
                tokio::io::AsyncReadExt::read_to_end(&mut file, &mut buf)
                    .await
                    .map_err(|e| RedfishError::GenericError {
                        error: format!("rune update_firmware: read image: {e}"),
                    })?;
                let image = BASE64.encode(&buf);
                let ctx = self.ctx().await?;
                self.call::<_, Task>("update_firmware", (ctx, image)).await
            })
        } else {
            Box::pin(async move { self.resolved().await?.update_firmware(filename).await })
        }
    }

    // Timeout crosses as whole seconds and the component as json text, since it has
    // variants carrying data rather than being one flat name.
    fn update_firmware_multipart<'a>(
        &'a self,
        firmware: &'a Path,
        reboot: bool,
        timeout: Duration,
        component_type: ComponentType,
    ) -> RedfishFuture<'a, Result<String, RedfishError>> {
        let scripted = self.has("update_firmware_multipart");
        trace_call!(
            "update_firmware_multipart",
            if scripted { "script" } else { "standard" }
        );
        if scripted {
            let path = firmware.to_string_lossy().to_string();
            let secs = timeout.as_secs() as i64;
            let component = serde_json::to_string(&component_type);
            Box::pin(async move {
                let component = component.map_err(|e| RedfishError::GenericError {
                    error: format!("rune update_firmware_multipart: {e}"),
                })?;
                let ctx = self.ctx().await?;
                self.call::<_, String>(
                    "update_firmware_multipart",
                    (ctx, path, reboot, secs, component),
                )
                .await
            })
        } else {
            Box::pin(async move {
                self.resolved()
                    .await?
                    .update_firmware_multipart(firmware, reboot, timeout, component_type)
                    .await
            })
        }
    }

    fn update_firmware_simple_update<'a>(
        &'a self,
        image_uri: &'a str,
        targets: Vec<String>,
        transfer_protocol: TransferProtocolType,
    ) -> RedfishFuture<'a, Result<Task, RedfishError>> {
        let scripted = self.has("update_firmware_simple_update");
        trace_call!(
            "update_firmware_simple_update",
            if scripted { "script" } else { "standard" }
        );
        if scripted {
            let uri = image_uri.to_string();
            let list = targets.clone();
            let protocol = serde_json::to_string(&transfer_protocol);
            Box::pin(async move {
                let protocol = protocol.map_err(|e| RedfishError::GenericError {
                    error: format!("rune update_firmware_simple_update: {e}"),
                })?;
                let ctx = self.ctx().await?;
                self.call::<_, Task>("update_firmware_simple_update", (ctx, uri, list, protocol))
                    .await
            })
        } else {
            Box::pin(async move {
                self.resolved()
                    .await?
                    .update_firmware_simple_update(image_uri, targets, transfer_protocol)
                    .await
            })
        }
    }

    // A rune Value is not Send so it cannot cross into the vm as an argument. Attributes
    // arrive as json text instead, which ctx.json_decode turns back into an object.
    fn set_bios<'a>(
        &'a self,
        values: HashMap<String, serde_json::Value>,
    ) -> RedfishFuture<'a, Result<(), RedfishError>> {
        let scripted = self.has("set_bios");
        trace_call!("set_bios", if scripted { "script" } else { "standard" });
        if scripted {
            let json = serde_json::to_string(&values);
            Box::pin(async move {
                let attrs = json.map_err(|e| RedfishError::GenericError {
                    error: format!("rune set_bios: {e}"),
                })?;
                let ctx = self.ctx().await?;
                self.call::<_, ()>("set_bios", (ctx, attrs)).await
            })
        } else {
            Box::pin(async move { self.resolved().await?.set_bios(values).await })
        }
    }

    fn get_network_device_function<'a>(
        &'a self,
        chassis_id: &'a str,
        id: &'a str,
        port: Option<&'a str>,
    ) -> RedfishFuture<'a, Result<NetworkDeviceFunction, RedfishError>> {
        let scripted = self.has("get_network_device_function");
        trace_call!(
            "get_network_device_function",
            if scripted { "script" } else { "standard" }
        );
        if scripted {
            let (chassis, func, port_id) = (
                chassis_id.to_string(),
                id.to_string(),
                port.map(str::to_string),
            );
            Box::pin(async move {
                let ctx = self.ctx().await?;
                self.call::<_, NetworkDeviceFunction>(
                    "get_network_device_function",
                    (ctx, chassis, func, port_id),
                )
                .await
            })
        } else {
            Box::pin(async move {
                self.resolved()
                    .await?
                    .get_network_device_function(chassis_id, id, port)
                    .await
            })
        }
    }

    // Mirrors get_collection, the script sees the path with the redfish prefix removed.
    fn get_resource<'a>(
        &'a self,
        id: ODataId,
    ) -> RedfishFuture<'a, Result<Resource, RedfishError>> {
        let scripted = self.has("get_resource");
        trace_call!("get_resource", if scripted { "script" } else { "standard" });
        if scripted {
            let url = id.odata_id.replace(&format!("/{REDFISH_ENDPOINT}/"), "");
            Box::pin(async move {
                let ctx = self.ctx().await?;
                let body = self
                    .call::<_, HashMap<String, serde_json::Value>>(
                        "get_resource",
                        (ctx, url.clone()),
                    )
                    .await?;
                let raw = serde_json::to_string(&body)
                    .and_then(serde_json::value::RawValue::from_string)
                    .map_err(|e| RedfishError::GenericError {
                        error: format!("rune get_resource: {e}"),
                    })?;
                Ok(Resource { url, raw })
            })
        } else {
            Box::pin(async move { self.resolved().await?.get_resource(id).await })
        }
    }

    fn get_collection<'a>(
        &'a self,
        id: ODataId,
    ) -> RedfishFuture<'a, Result<Collection, RedfishError>> {
        let scripted = self.has("get_collection");
        trace_call!(
            "get_collection",
            if scripted { "script" } else { "standard" }
        );
        if scripted {
            let url = id.odata_id.replace(&format!("/{REDFISH_ENDPOINT}/"), "");
            Box::pin(async move {
                let ctx = self.ctx().await?;
                let body = self
                    .call::<_, HashMap<String, serde_json::Value>>(
                        "get_collection",
                        (ctx, url.clone()),
                    )
                    .await?;
                Ok(Collection { url, body })
            })
        } else {
            Box::pin(async move { self.resolved().await?.get_collection(id).await })
        }
    }

    fn set_boot_order_dpu_first<'a>(
        &'a self,
        boot_interface: crate::BootInterfaceRef<'a>,
    ) -> RedfishFuture<'a, Result<Option<String>, RedfishError>> {
        let scripted = self.has("set_boot_order_dpu_first");
        trace_call!(
            "set_boot_order_dpu_first",
            if scripted { "script" } else { "standard" }
        );
        if scripted {
            Box::pin(async move {
                let ctx = self.ctx().await?;
                self.call::<_, Option<String>>("set_boot_order_dpu_first", (ctx,))
                    .await
            })
        } else {
            Box::pin(async move {
                self.resolved()
                    .await?
                    .set_boot_order_dpu_first(boot_interface)
                    .await
            })
        }
    }

    dispatch_display! {
        lockdown_bmc(target: EnabledDisabled) -> ();
        enable_ipmi_over_lan(target: EnabledDisabled) -> ();
        set_nic_mode(mode: NicMode) -> ();
        set_host_rshim(enabled: EnabledDisabled) -> ();
        set_idrac_lockdown(enabled: EnabledDisabled) -> ();
    }

    fn is_boot_order_setup<'a>(
        &'a self,
        boot_interface: crate::BootInterfaceRef<'a>,
    ) -> RedfishFuture<'a, Result<bool, RedfishError>> {
        let scripted = self.has("is_boot_order_setup");
        trace_call!(
            "is_boot_order_setup",
            if scripted { "script" } else { "standard" }
        );
        if scripted {
            Box::pin(async move {
                let ctx = self.ctx().await?;
                self.call::<_, bool>("is_boot_order_setup", (ctx,)).await
            })
        } else {
            Box::pin(async move {
                self.resolved()
                    .await?
                    .is_boot_order_setup(boot_interface)
                    .await
            })
        }
    }

    dispatch_display! {
        set_host_privilege_level(level: HostPrivilegeLevel) -> ();
    }

    fn ac_powercycle_supported_by_power(&self) -> bool {
        match self.ac_powercycle_supported.get() {
            Some(supported) => {
                trace_call!("ac_powercycle_supported_by_power", "script");
                *supported
            }
            None => {
                trace_call!("ac_powercycle_supported_by_power", "standard");
                self.snapshot().ac_powercycle_supported_by_power()
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::{
        compile, do_b64_decode, interpret, response_json, shared_runtime, RedfishCtx,
        RedfishHttpClient,
    };
    use reqwest::header::HeaderMap;
    use reqwest::StatusCode;
    use rune::runtime::Value;
    use rune::Vm;

    const STUB: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/rune/http_stub.rn");

    // Run a no argument async fn from the committed stub and return its raw rune Value.
    async fn run(name: &str) -> Value {
        let unit = compile(STUB).unwrap();
        Vm::new(shared_runtime().unwrap(), unit)
            .send_execute([name], ())
            .unwrap()
            .async_complete()
            .await
            .into_result()
            .unwrap()
    }

    // Compile a script, run an async fn, and bridge the result back through serde_json.
    #[tokio::test]
    async fn script_runs_and_result_bridges() {
        let path = std::env::temp_dir().join("libredfish_rune_ok.rn");
        std::fs::write(&path, "pub async fn answer() { 42 }").unwrap();
        let unit = compile(path.to_str().unwrap()).unwrap();
        let runtime = shared_runtime().unwrap();
        let value = Vm::new(runtime, unit)
            .send_execute(["answer"], ())
            .unwrap()
            .async_complete()
            .await
            .into_result()
            .unwrap();
        let n: i64 = serde_json::from_value(serde_json::to_value(&value).unwrap()).unwrap();
        assert_eq!(n, 42);
    }

    #[test]
    fn compile_error_is_file_error() {
        let path = std::env::temp_dir().join("libredfish_rune_bad.rn");
        std::fs::write(&path, "pub async fn x( {").unwrap();
        let err = compile(path.to_str().unwrap()).unwrap_err();
        assert!(
            matches!(err, crate::RedfishError::FileError(_)),
            "expected FileError, got {err:?}"
        );
    }

    #[test]
    fn json_value_bridge_roundtrips() {
        let j = serde_json::json!({"PowerState":"On","n":3,"list":[1,2],"nil":null});
        let v: Value = serde_json::from_value(j.clone()).unwrap();
        let back = serde_json::to_value(&v).unwrap();
        assert_eq!(j, back);
    }

    // Unit returns (from every dispatched method that yields `()`) must bridge.
    #[tokio::test]
    async fn unit_return_bridges() {
        let path = std::env::temp_dir().join("libredfish_rune_unit.rn");
        std::fs::write(&path, "pub async fn nothing() { () }").unwrap();
        let unit = compile(path.to_str().unwrap()).unwrap();
        let value = Vm::new(shared_runtime().unwrap(), unit)
            .send_execute(["nothing"], ())
            .unwrap()
            .async_complete()
            .await
            .into_result()
            .unwrap();
        let _: () = serde_json::from_value(serde_json::to_value(&value).unwrap()).unwrap();
    }

    // A script `Err(..)` (say from `?` on a failed call) becomes a RedfishError.
    #[tokio::test]
    async fn script_err_becomes_redfish_error() {
        let v = run("returns_err").await;
        let r = interpret::<()>(&v, "returns_err");
        assert!(
            matches!(r, Err(crate::RedfishError::GenericError { .. })),
            "expected GenericError, got {r:?}"
        );
    }

    // A top level `Ok(v)` is unwrapped before bridging.
    #[tokio::test]
    async fn script_ok_is_unwrapped() {
        let v = run("returns_ok").await;
        let n: i64 = interpret::<i64>(&v, "returns_ok").unwrap();
        assert_eq!(n, 42);
    }

    // A bare return that isn't a Result bridges directly (backward compatible).
    #[tokio::test]
    async fn bare_value_passes_through() {
        let v = run("returns_bare").await;
        let n: i64 = interpret::<i64>(&v, "returns_bare").unwrap();
        assert_eq!(n, 42);
    }

    // power_state and reset_and_wait run against a closed port, exercising the match
    // and `?` dispatch logic end to end, not just that the script compiles.
    #[tokio::test]
    async fn stub_http_functions_run_against_closed_port() {
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let endpoint = crate::Endpoint {
            host: format!("127.0.0.1:{port}"),
            port: None,
            user: None,
            password: None,
        };
        let client = RedfishHttpClient::new(reqwest::Client::new(), endpoint, Vec::new());
        let ctx = RedfishCtx {
            client,
            system_id: "1".to_string(),
            manager_id: "1".to_string(),
            variant: None,
            data: None,
        };
        let unit = compile(STUB).unwrap();
        let rt = shared_runtime().unwrap();

        let value = Vm::new(rt.clone(), unit.clone())
            .send_execute(["power_state"], (ctx.clone(),))
            .unwrap()
            .async_complete()
            .await
            .into_result()
            .unwrap();
        let power_state: String = interpret(&value, "power_state").unwrap();
        assert_eq!(
            power_state, "Unknown",
            "power_state falls back on a failed GET"
        );

        let value = Vm::new(rt, unit)
            .send_execute(["reset_and_wait"], (ctx,))
            .unwrap()
            .async_complete()
            .await
            .into_result()
            .unwrap();
        assert!(
            matches!(
                interpret::<Option<String>>(&value, "reset_and_wait"),
                Err(crate::RedfishError::GenericError { .. })
            ),
            "reset_and_wait's ? should fail the method on a failed POST"
        );
    }

    // The response object handed to scripts has status, headers, and body.
    #[test]
    fn response_json_shape() {
        let mut h = HeaderMap::new();
        h.insert(
            "location",
            "/redfish/v1/TaskService/Tasks/3".parse().unwrap(),
        );
        let body = Some(serde_json::json!({ "PowerState": "On" }));
        let j = response_json(StatusCode::ACCEPTED, Some(h), body);
        assert_eq!(j["status"], 202);
        assert_eq!(j["headers"]["location"], "/redfish/v1/TaskService/Tasks/3");
        assert_eq!(j["body"]["PowerState"], "On");
    }

    // The free helpers register and resolve when called bare, and `bmc_address` rides
    // on `ctx`. Offline, read_file hits a temp file and read_env a var set here.
    #[tokio::test]
    async fn host_helpers_register_and_run() {
        std::env::set_var("LIBREDFISH_RUNE_TEST_ENVVAR", "present");
        let file_path = std::env::temp_dir().join("libredfish_rune_readfile.txt");
        std::fs::write(&file_path, "hello-from-file").unwrap();

        let endpoint = crate::Endpoint {
            host: "bmc.example".to_string(),
            port: None,
            user: None,
            password: None,
        };
        let client = RedfishHttpClient::new(reqwest::Client::new(), endpoint, Vec::new());
        let ctx = RedfishCtx {
            client,
            system_id: "1".to_string(),
            manager_id: "1".to_string(),
            variant: None,
            data: None,
        };

        let script = r#"pub async fn probe(ctx, file_path, env_present, env_missing) {
    let decoded_b64 = match b64_decode("YWJj") { Ok(t) => t, Err(e) => e };
    let obj = match json_decode("{\"PowerState\":\"On\",\"n\":3}") { Ok(v) => v, Err(_) => #{} };
    let file = match read_file(file_path) { Ok(t) => t, Err(e) => e };
    let encoded = match json_encode(#{ "a": 1 }) { Ok(s) => s, Err(e) => e };
    #{
        "addr": ctx.bmc_address(),
        "sha256_abc": sha256("abc"),
        "sha512_abc": sha512("abc"),
        "b64_abc": b64_encode("abc"),
        "b64_roundtrip": decoded_b64,
        "json_power": obj["PowerState"],
        "json_n": obj["n"],
        "json_encoded": encoded,
        "file": file,
        "env_present": read_env(env_present),
        "env_missing": read_env(env_missing),
        "unix_time": unix_time()
    }
}"#;
        let path = std::env::temp_dir().join("libredfish_rune_host_helpers.rn");
        std::fs::write(&path, script).unwrap();

        let unit = compile(path.to_str().unwrap()).unwrap();
        let value = Vm::new(shared_runtime().unwrap(), unit)
            .send_execute(
                ["probe"],
                (
                    ctx,
                    file_path.to_string_lossy().to_string(),
                    "LIBREDFISH_RUNE_TEST_ENVVAR".to_string(),
                    "LIBREDFISH_RUNE_DEFINITELY_UNSET_9f3b".to_string(),
                ),
            )
            .unwrap()
            .async_complete()
            .await
            .into_result()
            .unwrap();
        let out: serde_json::Value = serde_json::to_value(&value).unwrap();

        assert_eq!(out["addr"], "bmc.example");
        // NIST sha256 and sha512 vectors for "abc", proving the digest, not registration.
        assert_eq!(
            out["sha256_abc"],
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            out["sha512_abc"],
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
        assert_eq!(out["b64_abc"], "YWJj");
        assert_eq!(out["b64_roundtrip"], "abc");
        assert_eq!(out["json_power"], "On");
        assert_eq!(out["json_n"], 3);
        assert_eq!(out["json_encoded"], "{\"a\":1}");
        assert_eq!(out["file"], "hello-from-file");
        assert_eq!(out["env_present"], "present");
        assert!(out["env_missing"].is_null());
        assert!(
            out["unix_time"].as_i64().unwrap() > 1_700_000_000,
            "unix_time should be a recent epoch second, got {:?}",
            out["unix_time"]
        );
    }

    // `b64_decode` round trips valid input and reports an error on invalid base64 (the Err
    // the script can `match`/`?`).
    #[test]
    fn b64_decode_roundtrips_and_rejects_invalid() {
        assert_eq!(do_b64_decode("YWJj").unwrap(), "abc");
        assert!(do_b64_decode("*** not base64 ***").is_err());
    }

    /// Every method on `Redfish` must be reachable from a script, via a dispatch macro
    /// or a hand written branch. A new trait method with no script path fails here.
    #[test]
    fn every_trait_method_is_script_overridable() {
        const TRAIT_SRC: &str = include_str!("lib.rs");
        const DISPATCH_SRC: &str = include_str!("rune_vendor.rs");

        let body = TRAIT_SRC
            .split_once("pub trait Redfish")
            .expect("`pub trait Redfish` not found in lib.rs")
            .1;

        let mut names: Vec<String> = Vec::new();
        for line in body.lines() {
            if line == "}" {
                break;
            }
            if let Some(rest) = line.strip_prefix("    fn ") {
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if !name.is_empty() {
                    names.push(name);
                }
            }
        }
        assert!(
            names.len() > 90,
            "parsed only {} trait methods, the parser is probably wrong",
            names.len()
        );

        let dispatched = |name: &str| {
            if DISPATCH_SRC.contains(&format!("self.has(\"{name}\")")) {
                return true;
            }
            // Entries inside dispatch macros name a method and its return type.
            DISPATCH_SRC.lines().any(|line| {
                let trimmed = line.trim_start();
                trimmed
                    .strip_prefix(name)
                    .is_some_and(|after| after.starts_with(" ->") || after.starts_with('('))
            })
        };

        let missing: Vec<&String> = names.iter().filter(|n| !dispatched(n)).collect();
        assert!(
            missing.is_empty(),
            "{} trait method(s) cannot be overridden from rune: {missing:?}",
            missing.len()
        );
    }

    /// The names the dispatch layer looks up must be the names a script can actually
    /// define. Covers one method per argument bridge added for script dispatch.
    #[tokio::test]
    async fn script_defined_overrides_are_found_by_lookup() {
        let src = "
pub async fn set_idrac_lockdown(ctx, enabled) { () }
pub async fn set_bios(ctx, attributes_json) { () }
pub async fn update_firmware(ctx, image_b64) { () }
pub async fn change_boot_order(ctx, order) { () }
pub async fn get_bmc_event_log(ctx, since) { [] }
pub async fn ac_powercycle_supported_by_power(ctx) { true }
";
        let path = std::env::temp_dir().join("libredfish_rune_dispatch_names.rn");
        std::fs::write(&path, src).unwrap();
        let unit = compile(path.to_str().unwrap()).unwrap();
        let runtime = shared_runtime().unwrap();

        for name in [
            "set_idrac_lockdown",
            "set_bios",
            "update_firmware",
            "change_boot_order",
            "get_bmc_event_log",
            "ac_powercycle_supported_by_power",
        ] {
            assert!(
                Vm::new(runtime.clone(), unit.clone())
                    .lookup_function([name])
                    .is_ok(),
                "script defined {name} but lookup_function could not find it"
            );
        }

        // A name the script does not define must not resolve, otherwise the `has` check
        // would report every method as scripted and never fall back to standard.
        assert!(
            Vm::new(runtime, unit)
                .lookup_function(["not_defined_anywhere"])
                .is_err(),
            "lookup_function resolved a name the script never defined"
        );
    }
}

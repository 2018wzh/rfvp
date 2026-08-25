use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard, OnceLock},
};

use abi_stable::std_types::RString;
use astra_emu_family_api::{
    validate_symbol, FfiLegacyFamilyHostAdapter, FfiLegacyHostServices, FfiOpenCall, FfiProbeCall,
    FfiProbeReport, FfiProviderInstanceRequest, FfiSessionCall, FfiShutdownReport, FfiStepCall,
    FfiStepOutput, LegacyProviderError, LegacyRuntimeProvider, LegacyRuntimeSessionId,
};

use crate::FvpRuntimeProvider;

type SharedProvider = Arc<Mutex<FvpRuntimeProvider>>;

static PROVIDERS: OnceLock<Mutex<BTreeMap<String, SharedProvider>>> = OnceLock::new();

pub fn create_instance(
    services: FfiLegacyHostServices,
    request: FfiProviderInstanceRequest,
) -> Result<(), LegacyProviderError> {
    let instance_id = request.instance_id.to_string();
    validate_symbol("instance_id", &instance_id)?;
    let mut providers = lock_registry()?;
    if providers.contains_key(&instance_id) {
        return Err(LegacyProviderError::invalid(
            "ASTRA_FVP_INSTANCE_DUPLICATE",
            "provider instance id is already active",
        ));
    }
    let host = FfiLegacyFamilyHostAdapter::new(services).into_host_services();
    providers.insert(
        instance_id,
        Arc::new(Mutex::new(FvpRuntimeProvider::with_host(host))),
    );
    Ok(())
}

pub fn destroy_instance(request: FfiProviderInstanceRequest) -> Result<(), LegacyProviderError> {
    let instance_id = request.instance_id.to_string();
    let mut providers = lock_registry()?;
    let provider = providers
        .get(&instance_id)
        .cloned()
        .ok_or_else(instance_missing)?;
    if lock_provider(&provider)?.has_active_sessions() {
        return Err(LegacyProviderError::invalid(
            "ASTRA_FVP_INSTANCE_ACTIVE_SESSIONS",
            "provider instance still owns active sessions",
        ));
    }
    providers.remove(&instance_id);
    Ok(())
}

pub fn probe(call: FfiProbeCall) -> Result<FfiProbeReport, LegacyProviderError> {
    let provider = provider(call.instance_id.as_str())?;
    let result = lock_provider(&provider)?
        .probe(&call.ctx.into(), call.request.into())
        .map(Into::into);
    result
}

pub fn open(call: FfiOpenCall) -> Result<RString, LegacyProviderError> {
    let provider = provider(call.instance_id.as_str())?;
    let result = lock_provider(&provider)?
        .open(&call.ctx.into(), call.request.try_into()?)
        .map(|session| session.0.into());
    result
}

pub fn step(call: FfiStepCall) -> Result<FfiStepOutput, LegacyProviderError> {
    let provider = provider(call.instance_id.as_str())?;
    let output = lock_provider(&provider)?.step(
        &call.ctx.into(),
        &LegacyRuntimeSessionId(call.session_id.to_string()),
        call.input.into(),
    )?;
    FfiStepOutput::try_from(output)
}

pub fn shutdown(call: FfiSessionCall) -> Result<FfiShutdownReport, LegacyProviderError> {
    let provider = provider(call.instance_id.as_str())?;
    let result = lock_provider(&provider)?
        .shutdown(
            &call.ctx.into(),
            &LegacyRuntimeSessionId(call.session_id.to_string()),
        )
        .map(Into::into);
    result
}

fn provider(instance_id: &str) -> Result<SharedProvider, LegacyProviderError> {
    lock_registry()?
        .get(instance_id)
        .cloned()
        .ok_or_else(instance_missing)
}

fn lock_registry(
) -> Result<MutexGuard<'static, BTreeMap<String, SharedProvider>>, LegacyProviderError> {
    PROVIDERS
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .map_err(|_| lock_error())
}

fn lock_provider(
    provider: &SharedProvider,
) -> Result<MutexGuard<'_, FvpRuntimeProvider>, LegacyProviderError> {
    provider.lock().map_err(|_| lock_error())
}

fn lock_error() -> LegacyProviderError {
    LegacyProviderError::invalid(
        "ASTRA_FVP_INSTANCE_LOCK_POISONED",
        "provider instance registry lock is poisoned",
    )
}

fn instance_missing() -> LegacyProviderError {
    LegacyProviderError::invalid(
        "ASTRA_FVP_INSTANCE_MISSING",
        "provider instance id is not active",
    )
}

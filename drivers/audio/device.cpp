#define OU_UNIT 1
#include "audio.h"
#include <wdmsec.h>

static EVT_WDF_DRIVER_DEVICE_ADD AddDevice;
static EVT_WDF_DEVICE_PREPARE_HARDWARE Prepare;
static EVT_WDF_DEVICE_RELEASE_HARDWARE Release;
static EVT_WDF_DEVICE_D0_EXIT PowerExit;

void RecordFailure(ULONG unit, ULONG line, NTSTATUS status) {
    if (KeGetCurrentIrql() != PASSIVE_LEVEL) return;
    WDFKEY key;
    auto opened = WdfDriverOpenParametersRegistryKey(WdfGetDriver(), KEY_QUERY_VALUE | KEY_SET_VALUE, WDF_NO_OBJECT_ATTRIBUTES, &key);
    if (!NT_SUCCESS(opened)) return;
    UNICODE_STRING code = RTL_CONSTANT_STRING(L"LastFailureStatus");
    UNICODE_STRING source = RTL_CONSTANT_STRING(L"LastFailureUnit");
    UNICODE_STRING location = RTL_CONSTANT_STRING(L"LastFailureLine");
    ULONG previous = 0;
    auto queried = WdfRegistryQueryULong(key, &code, &previous);
    if (!NT_SUCCESS(queried) || status == STATUS_SUCCESS || !previous) {
        (void)WdfRegistryAssignULong(key, &code, (ULONG)status);
        (void)WdfRegistryAssignULong(key, &source, unit);
        (void)WdfRegistryAssignULong(key, &location, line);
    }
    WdfRegistryClose(key);
}

extern "C" NTSTATUS DriverEntry(PDRIVER_OBJECT driver, PUNICODE_STRING registry) {
    WDF_DRIVER_CONFIG config;
    WDF_DRIVER_CONFIG_INIT(&config, AddDevice);
    WDFDRIVER handle;
    OU_TRY(WdfDriverCreate(driver, registry, WDF_NO_OBJECT_ATTRIBUTES, &config, &handle));
    RecordFailure(0, 0, STATUS_SUCCESS);
    ACX_DRIVER_CONFIG acx;
    ACX_DRIVER_CONFIG_INIT(&acx);
    auto status = AcxDriverInitialize(handle, &acx);
    if (!NT_SUCCESS(status)) RecordFailure(1, __LINE__, status);
    return status;
}

static NTSTATUS AddDevice(WDFDRIVER, PWDFDEVICE_INIT init) {
    ACX_DEVICEINIT_CONFIG acxInit;
    ACX_DEVICEINIT_CONFIG_INIT(&acxInit);
    OU_TRY(AcxDeviceInitInitialize(init, &acxInit));
    WDF_PNPPOWER_EVENT_CALLBACKS callbacks;
    WDF_PNPPOWER_EVENT_CALLBACKS_INIT(&callbacks);
    callbacks.EvtDevicePrepareHardware = Prepare;
    callbacks.EvtDeviceReleaseHardware = Release;
    callbacks.EvtDeviceD0Exit = PowerExit;
    WdfDeviceInitSetPnpPowerEventCallbacks(init, &callbacks);
    WDF_OBJECT_ATTRIBUTES attributes;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes, AudioDevice);
    WDFDEVICE device;
    OU_TRY(WdfDeviceCreate(&init, &attributes, &device));
    auto ctx = DeviceContext(device);
    KeInitializeSpinLock(&ctx->Lock);
    ctx->State.Abi = OU_AUDIO_ABI;
    ctx->State.Generation = 1;
    ACX_DEVICE_CONFIG config;
    ACX_DEVICE_CONFIG_INIT(&config);
    OU_TRY(AcxDeviceInitialize(device, &config));
    OU_TRY(CreateCircuit(device, FALSE, &ctx->Render));
    OU_TRY(CreateCircuit(device, TRUE, &ctx->Capture));
    return STATUS_SUCCESS;
}

static NTSTATUS Prepare(WDFDEVICE device, WDFCMRESLIST, WDFCMRESLIST) {
    auto ctx = DeviceContext(device);
    WDF_DEVICE_POWER_POLICY_IDLE_SETTINGS idle;
    WDF_DEVICE_POWER_POLICY_IDLE_SETTINGS_INIT(&idle, IdleCannotWakeFromS0);
    idle.IdleTimeout = 5000;
    idle.IdleTimeoutType = SystemManagedIdleTimeoutWithHint;
    idle.ExcludeD3Cold = WdfTrue;
    OU_TRY(WdfDeviceAssignS0IdleSettings(device, &idle));
    if (!ctx->RenderAdded) {
        OU_TRY(AcxDeviceAddCircuit(device, ctx->Render));
        ctx->RenderAdded = TRUE;
    }
    if (!ctx->CaptureAdded) {
        OU_TRY(AcxDeviceAddCircuit(device, ctx->Capture));
        ctx->CaptureAdded = TRUE;
    }
    if (!ctx->Bridge) OU_TRY(CreateBridge(device));
    KIRQL irql;
    KeAcquireSpinLock(&ctx->Lock, &irql);
    ctx->Online = TRUE;
    KeReleaseSpinLock(&ctx->Lock, irql);
    return STATUS_SUCCESS;
}

static NTSTATUS PowerExit(WDFDEVICE device, WDF_POWER_DEVICE_STATE) {
    ClearBridge(DeviceContext(device), false);
    return STATUS_SUCCESS;
}

static NTSTATUS Release(WDFDEVICE device, WDFCMRESLIST) {
    auto ctx = DeviceContext(device);
    KIRQL irql;
    KeAcquireSpinLock(&ctx->Lock, &irql);
    ctx->Online = FALSE;
    KeReleaseSpinLock(&ctx->Lock, irql);
    // ACX removes/stops streams before the bridge device can be destroyed.
    NTSTATUS result = STATUS_SUCCESS;
    if (ctx->CaptureAdded) {
        result = AcxDeviceRemoveCircuit(device, ctx->Capture);
        if (NT_SUCCESS(result)) ctx->CaptureAdded = FALSE;
    }
    if (ctx->RenderAdded) {
        auto status = AcxDeviceRemoveCircuit(device, ctx->Render);
        if (!NT_SUCCESS(status) && NT_SUCCESS(result)) result = status;
        if (NT_SUCCESS(status)) ctx->RenderAdded = FALSE;
    }
    ClearBridge(ctx);
    if (ctx->Bridge) {
        KeAcquireSpinLock(&ctx->Lock, &irql);
        auto waits = ctx->Waits;
        auto bridge = ctx->Bridge;
        ctx->Bridge = nullptr;
        ctx->Waits = nullptr;
        KeReleaseSpinLock(&ctx->Lock, irql);
        WdfIoQueuePurgeSynchronously(waits);
        WdfObjectDelete(bridge);
    }
    return result;
}

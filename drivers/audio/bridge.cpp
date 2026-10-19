#define OU_UNIT 4
#include "audio.h"
#include <wdmsec.h>

ULONG Ring::Read(short* out, ULONG frames) {
    ULONG count = min(frames, Size);
    ULONG first = min(count, OU_AUDIO_CAPACITY - Head);
    RtlCopyMemory(out, Samples + Head * 2, first * 4);
    RtlCopyMemory(out + first * 2, Samples, (count - first) * 4);
    Head = (Head + count) % OU_AUDIO_CAPACITY;
    Size -= count;
    return count;
}
ULONG Ring::Write(const short* input, ULONG frames) {
    ULONG discarded = 0;
    if (frames > OU_AUDIO_CAPACITY) {
        discarded = frames - OU_AUDIO_CAPACITY;
        input += discarded * 2;
        frames = OU_AUDIO_CAPACITY;
    }
    if (Size + frames > OU_AUDIO_CAPACITY) {
        ULONG drop = Size + frames - OU_AUDIO_CAPACITY;
        Head = (Head + drop) % OU_AUDIO_CAPACITY;
        Size -= drop;
        discarded += drop;
    }
    ULONG tail = (Head + Size) % OU_AUDIO_CAPACITY;
    ULONG first = min(frames, OU_AUDIO_CAPACITY - tail);
    RtlCopyMemory(Samples + tail * 2, input, first * 4);
    RtlCopyMemory(Samples, input + first * 2, (frames - first) * 4);
    Size += frames;
    return discarded;
}

static OU_AUDIO_STATE Snapshot(AudioDevice* ctx) {
    KIRQL irql;
    KeAcquireSpinLock(&ctx->Lock, &irql);
    auto result = ctx->State;
    result.SpeakerFrames = ctx->Speaker.Size;
    result.MicrophoneFrames = ctx->Microphone.Size;
    KeReleaseSpinLock(&ctx->Lock, irql);
    return result;
}

static void CompleteState(WDFREQUEST request, AudioDevice* ctx) {
    OU_AUDIO_STATE* output = nullptr;
    auto status = WdfRequestRetrieveOutputBuffer(request, sizeof(*output), (PVOID*)&output, nullptr);
    if (NT_SUCCESS(status)) *output = Snapshot(ctx);
    WdfRequestCompleteWithInformation(request, status, NT_SUCCESS(status) ? sizeof(*output) : 0);
}

struct QueueReference {
    WDFQUEUE Queue;
    explicit QueueReference(AudioDevice* ctx) {
        KIRQL irql;
        KeAcquireSpinLock(&ctx->Lock, &irql);
        Queue = ctx->Waits;
        if (Queue) WdfObjectReference(Queue);
        KeReleaseSpinLock(&ctx->Lock, irql);
    }
    ~QueueReference() { if (Queue) WdfObjectDereference(Queue); }
};

void WakeBridge(AudioDevice* ctx) {
    QueueReference queue(ctx);
    if (!queue.Queue) return;
    WDFREQUEST request;
    while (NT_SUCCESS(WdfIoQueueRetrieveNextRequest(queue.Queue, &request))) {
        CompleteState(request, ctx);
    }
}

void ClearBridge(AudioDevice* ctx, bool revoke) {
    KIRQL irql;
    KeAcquireSpinLock(&ctx->Lock, &irql);
    if (revoke) ctx->State.Flags = 0;
    ctx->State.Generation++;
    ctx->State.Sequence++;
    ctx->Speaker.Clear();
    ctx->Microphone.Clear();
    KeReleaseSpinLock(&ctx->Lock, irql);
    WakeBridge(ctx);
}

static EVT_WDF_IO_QUEUE_IO_DEVICE_CONTROL Control;
static EVT_WDF_DEVICE_FILE_CREATE FileCreate;
static EVT_WDF_FILE_CLEANUP FileCleanup;
static EVT_WDF_OBJECT_CONTEXT_DESTROY BridgeDestroy;

static void FileCreate(WDFDEVICE device, WDFREQUEST request, WDFFILEOBJECT file) {
    auto status = WdfDeviceStopIdle(BridgeData(device)->Parent, TRUE);
    if (NT_SUCCESS(status)) {
        auto ctx = DeviceContext(BridgeData(device)->Parent);
        KIRQL irql;
        KeAcquireSpinLock(&ctx->Lock, &irql);
        BridgeFileData(file)->PowerHeld = TRUE;
        BridgeFileData(file)->Active = ctx->Online;
        if (!ctx->Online) status = STATUS_DEVICE_NOT_READY;
        KeReleaseSpinLock(&ctx->Lock, irql);
        if (!NT_SUCCESS(status)) {
            BridgeFileData(file)->PowerHeld = FALSE;
            WdfDeviceResumeIdle(BridgeData(device)->Parent);
        }
    }
    WdfRequestComplete(request, status);
}

static void FileCleanup(WDFFILEOBJECT file) {
    auto ctx = DeviceContext(BridgeData(WdfFileObjectGetDevice(file))->Parent);
    KIRQL irql;
    KeAcquireSpinLock(&ctx->Lock, &irql);
    // Cleanup can race with a dispatched IOCTL. Revoke the file's lease and
    // PCM together; every mutation rechecks it while holding this same lock.
    if (BridgeFileData(file)->Active) {
        BridgeFileData(file)->Active = FALSE;
        ctx->State.Flags = 0;
        ctx->State.Generation++;
        ctx->State.Sequence++;
        ctx->Speaker.Clear();
        ctx->Microphone.Clear();
    }
    KeReleaseSpinLock(&ctx->Lock, irql);
    WakeBridge(ctx);
    if (BridgeFileData(file)->PowerHeld) {
        BridgeFileData(file)->PowerHeld = FALSE;
        WdfDeviceResumeIdle(BridgeData(WdfFileObjectGetDevice(file))->Parent);
    }
}

static void BridgeDestroy(WDFOBJECT object) {
    auto parent = BridgeData(object)->Parent;
    if (parent) WdfObjectDereference(parent);
}

NTSTATUS CreateBridge(WDFDEVICE parent) {
    auto ctx = DeviceContext(parent);
    // Only the privileged local host (or an explicitly elevated diagnostic)
    // can inject PCM. Ordinary applications use the standard audio endpoints.
    UNICODE_STRING security = RTL_CONSTANT_STRING(L"D:P(A;;GA;;;SY)(A;;GA;;;BA)");
    auto init = WdfControlDeviceInitAllocate(WdfDeviceGetDriver(parent), &security);
    if (!init) return STATUS_INSUFFICIENT_RESOURCES;
    UNICODE_STRING name = RTL_CONSTANT_STRING(L"\\Device\\FarwingAudioBridge");
    auto status = WdfDeviceInitAssignName(init, &name);
    if (!NT_SUCCESS(status)) { WdfDeviceInitFree(init); return status; }
    WdfDeviceInitSetExclusive(init, TRUE);
    WdfDeviceInitSetIoType(init, WdfDeviceIoBuffered);
    WDF_FILEOBJECT_CONFIG files;
    WDF_FILEOBJECT_CONFIG_INIT(&files, FileCreate, nullptr, FileCleanup);
    WDF_OBJECT_ATTRIBUTES fileAttributes;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&fileAttributes, BridgeFileContext);
    WdfDeviceInitSetFileObjectConfig(init, &files, &fileAttributes);
    WDF_OBJECT_ATTRIBUTES attributes;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes, BridgeContext);
    attributes.ExecutionLevel = WdfExecutionLevelPassive;
    attributes.EvtDestroyCallback = BridgeDestroy;
    WDFDEVICE bridge;
    status = WdfDeviceCreate(&init, &attributes, &bridge);
    if (!NT_SUCCESS(status)) { if (init) WdfDeviceInitFree(init); return status; }
    WdfObjectReference(parent);
    BridgeData(bridge)->Parent = parent;
    UNICODE_STRING link = RTL_CONSTANT_STRING(L"\\DosDevices\\Global\\FarwingAudioBridge");
    status = WdfDeviceCreateSymbolicLink(bridge, &link);
    WDF_IO_QUEUE_CONFIG queue;
    if (NT_SUCCESS(status)) {
        WDF_IO_QUEUE_CONFIG_INIT_DEFAULT_QUEUE(&queue, WdfIoQueueDispatchSequential);
        queue.PowerManaged = WdfFalse;
        queue.EvtIoDeviceControl = Control;
        status = WdfIoQueueCreate(bridge, &queue, WDF_NO_OBJECT_ATTRIBUTES, WDF_NO_HANDLE);
    }
    WDFQUEUE waits = nullptr;
    if (NT_SUCCESS(status)) {
        WDF_IO_QUEUE_CONFIG_INIT(&queue, WdfIoQueueDispatchManual);
        queue.PowerManaged = WdfFalse;
        status = WdfIoQueueCreate(bridge, &queue, WDF_NO_OBJECT_ATTRIBUTES, &waits);
    }
    if (!NT_SUCCESS(status)) { WdfObjectDelete(bridge); return status; }
    KIRQL irql;
    KeAcquireSpinLock(&ctx->Lock, &irql);
    ctx->Bridge = bridge;
    ctx->Waits = waits;
    ctx->Online = TRUE;
    KeReleaseSpinLock(&ctx->Lock, irql);
    WdfControlFinishInitializing(bridge);
    return STATUS_SUCCESS;
}

static void Control(WDFQUEUE queue, WDFREQUEST request, size_t, size_t, ULONG code) {
    auto ctx = DeviceContext(BridgeData(WdfIoQueueGetDevice(queue))->Parent);
    auto file = BridgeFileData(WdfRequestGetFileObject(request));
    NTSTATUS status = STATUS_INVALID_DEVICE_REQUEST;
    ULONG_PTR written = 0;
    KIRQL irql;
    KeAcquireSpinLock(&ctx->Lock, &irql);
    auto online = ctx->Online && file->Active;
    KeReleaseSpinLock(&ctx->Lock, irql);
    if (!online) { WdfRequestComplete(request, STATUS_DEVICE_NOT_READY); return; }
    if (code == OU_AUDIO_STATUS) { CompleteState(request, ctx); return; }
    if (code == OU_AUDIO_ENABLE) {
        OU_AUDIO_CONFIG* config;
        status = WdfRequestRetrieveInputBuffer(request, sizeof(*config), (PVOID*)&config, nullptr);
        if (NT_SUCCESS(status)) {
            if (config->Abi != OU_AUDIO_ABI || (config->Flags & ~3u)) status = STATUS_INVALID_PARAMETER;
            else {
                KeAcquireSpinLock(&ctx->Lock, &irql);
                if (!ctx->Online || !file->Active) status = STATUS_DEVICE_NOT_READY;
                else if (ctx->State.Flags != config->Flags) {
                    auto changed = ctx->State.Flags ^ config->Flags;
                    ctx->State.Flags = config->Flags;
                    ctx->State.Sequence++;
                    if (changed & OU_AUDIO_SPEAKER) ctx->Speaker.Clear();
                    if (changed & OU_AUDIO_MICROPHONE) {
                        ctx->State.Generation++;
                        ctx->Microphone.Clear();
                    }
                }
                KeReleaseSpinLock(&ctx->Lock, irql);
                WakeBridge(ctx);
            }
        }
    } else if (code == OU_AUDIO_READ) {
        OU_AUDIO_PCM* pcm;
        status = WdfRequestRetrieveOutputBuffer(request, sizeof(*pcm), (PVOID*)&pcm, nullptr);
        if (NT_SUCCESS(status)) {
            RtlZeroMemory(pcm, sizeof(*pcm));
            pcm->Abi = OU_AUDIO_ABI;
            KeAcquireSpinLock(&ctx->Lock, &irql);
            if (!ctx->Online || !file->Active) status = STATUS_DEVICE_NOT_READY;
            else {
                pcm->Generation = ctx->State.Generation;
                pcm->Frames = ctx->Speaker.Read(pcm->Samples, OU_AUDIO_BLOCK);
            }
            KeReleaseSpinLock(&ctx->Lock, irql);
            if (NT_SUCCESS(status)) written = sizeof(*pcm);
        }
    } else if (code == OU_AUDIO_WRITE) {
        OU_AUDIO_PCM* pcm;
        size_t length;
        status = WdfRequestRetrieveInputBuffer(request, FIELD_OFFSET(OU_AUDIO_PCM, Samples), (PVOID*)&pcm, &length);
        if (NT_SUCCESS(status)) {
            if (pcm->Abi != OU_AUDIO_ABI || !pcm->Frames || pcm->Frames > OU_AUDIO_BLOCK ||
                length < FIELD_OFFSET(OU_AUDIO_PCM, Samples) + pcm->Frames * 4) status = STATUS_INVALID_PARAMETER;
            else {
                KeAcquireSpinLock(&ctx->Lock, &irql);
                if (!ctx->Online || !file->Active) status = STATUS_DEVICE_NOT_READY;
                else if (pcm->Generation != ctx->State.Generation) status = STATUS_REVISION_MISMATCH;
                else if (!(ctx->State.Flags & OU_AUDIO_MICROPHONE) || !ctx->State.MicrophoneRunning)
                    status = STATUS_DEVICE_NOT_READY;
                else if (pcm->Frames > OU_AUDIO_CAPACITY - ctx->Microphone.Size)
                    status = STATUS_BUFFER_OVERFLOW; // Never queue beyond the advertised budget.
                else ctx->Microphone.Write(pcm->Samples, pcm->Frames);
                KeReleaseSpinLock(&ctx->Lock, irql);
            }
        }
    } else if (code == OU_AUDIO_WAIT) {
        OU_AUDIO_STATE* previous;
        status = WdfRequestRetrieveInputBuffer(request, sizeof(*previous), (PVOID*)&previous, nullptr);
        if (NT_SUCCESS(status)) {
            auto sequence = previous->Sequence;
            if (previous->Abi != OU_AUDIO_ABI) status = STATUS_REVISION_MISMATCH;
            else if (sequence != Snapshot(ctx).Sequence) { CompleteState(request, ctx); return; }
            else {
                QueueReference waiting(ctx);
                if (!waiting.Queue) { WdfRequestComplete(request, STATUS_DEVICE_NOT_READY); return; }
                ULONG queued = 0;
                WdfIoQueueGetState(waiting.Queue, &queued, nullptr);
                if (queued) status = STATUS_DEVICE_BUSY;
                else {
                    status = WdfRequestForwardToIoQueue(request, waiting.Queue);
                    if (NT_SUCCESS(status)) {
                        // Close the enqueue-vs-DPC race. The manual queue owns
                        // cancellation and file cleanup; no raw pending IRP.
                        if (sequence != Snapshot(ctx).Sequence) WakeBridge(ctx);
                        return;
                    }
                }
            }
        }
    }
    WdfRequestCompleteWithInformation(request, status, written);
}

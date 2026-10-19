// ACX streaming DDI adapted from Microsoft's sample; no sample tone/file I/O.
#include "audio.h"

static EVT_ACX_STREAM_ALLOCATE_RTPACKETS Allocate;
static EVT_ACX_STREAM_FREE_RTPACKETS Free;
static EVT_ACX_STREAM_PREPARE_HARDWARE PrepareStream;
static EVT_ACX_STREAM_RELEASE_HARDWARE ReleaseStream;
static EVT_ACX_STREAM_RUN Run;
static EVT_ACX_STREAM_PAUSE Pause;
static EVT_ACX_STREAM_GET_HW_LATENCY Latency;
static EVT_ACX_STREAM_GET_CURRENT_PACKET CurrentPacket;
static EVT_ACX_STREAM_GET_PRESENTATION_POSITION Position;
static EVT_ACX_STREAM_SET_RENDER_PACKET RenderPacket;
static EVT_ACX_STREAM_GET_CAPTURE_PACKET CapturePacket;
static EVT_ACX_STREAM_ASSIGN_DRM_CONTENT_ID Drm;
static EVT_WDF_TIMER Tick;
static EVT_WDF_OBJECT_CONTEXT_CLEANUP StreamCleanup;
static EVT_WDF_OBJECT_CONTEXT_CLEANUP TimerCleanup;

static ULONGLONG ClockFrames(StreamContext* ctx, ULONGLONG qpc) {
    ULONGLONG elapsed = qpc - ctx->AnchorQpc;
    return ctx->AnchorPosition + (elapsed / ctx->Frequency) * OU_AUDIO_RATE
        + (elapsed % ctx->Frequency) * OU_AUDIO_RATE / ctx->Frequency;
}

static ULONG Step(StreamContext* ctx) {
    // Timer-driven clients must see progress inside a large single buffer.
    ULONG remaining = ctx->Frames - (ULONG)(ctx->Position % ctx->Frames);
    return min(120u, remaining);
}

static void Schedule(StreamContext* ctx) {
    ULONGLONG targetFrames = ctx->Position + Step(ctx) - ctx->AnchorPosition;
    ULONGLONG target = ctx->AnchorQpc + targetFrames * ctx->Frequency / OU_AUDIO_RATE;
    ULONGLONG now = KeQueryPerformanceCounter(nullptr).QuadPart;
    LONGLONG hns = target > now ? (LONGLONG)((target - now) * 10000000 / ctx->Frequency) : 1;
    // Absolute sample timeline, rather than period-after-callback, prevents drift.
    WdfTimerStart(ctx->Timer, -max(1ll, hns));
}

static void PublishRunning(StreamContext* stream, BOOLEAN running) {
    auto ctx = DeviceContext(stream->Device);
    KIRQL irql;
    KeAcquireSpinLock(&ctx->Lock, &irql);
    if (stream->Capture) {
        ctx->State.MicrophoneRunning = running;
        ctx->Microphone.Clear();
        ctx->State.Generation++;
    } else {
        ctx->State.SpeakerRunning = running;
        ctx->Speaker.Clear();
    }
    ctx->State.Sequence++;
    KeReleaseSpinLock(&ctx->Lock, irql);
    WakeBridge(ctx);
}

NTSTATUS CreateStream(WDFDEVICE device, ACXCIRCUIT circuit, ACXPIN pin,
    PACXSTREAM_INIT init, ACXDATAFORMAT format, const GUID* mode, ACXOBJECTBAG) {
    if (AcxPinGetId(pin) != 0 || !mode ||
        (!IsEqualGUID(*mode, AUDIO_SIGNALPROCESSINGMODE_RAW) &&
         !IsEqualGUID(*mode, AUDIO_SIGNALPROCESSINGMODE_DEFAULT))) return STATUS_NOT_SUPPORTED;
    auto wave = (PWAVEFORMATEXTENSIBLE)AcxDataFormatGetWaveFormatExtensible(format);
    if (!wave || wave->Format.nSamplesPerSec != OU_AUDIO_RATE || wave->Format.nChannels != 2 ||
        wave->Format.wBitsPerSample != 16 || wave->Format.nBlockAlign != 4 ||
        !IsEqualGUID(wave->SubFormat, KSDATAFORMAT_SUBTYPE_PCM)) return STATUS_NO_MATCH;
    auto circuitCtx = CircuitData(circuit);
    // Windows audio engine provides shared-mode mixing/fan-out. Multiple
    // independent hardware clocks on one virtual endpoint are not supported.
    if (InterlockedCompareExchange(&circuitCtx->Streams, 1, 0)) return STATUS_DEVICE_BUSY;
    ACX_STREAM_CALLBACKS callbacks;
    ACX_STREAM_CALLBACKS_INIT(&callbacks);
    callbacks.EvtAcxStreamPrepareHardware = PrepareStream;
    callbacks.EvtAcxStreamReleaseHardware = ReleaseStream;
    callbacks.EvtAcxStreamRun = Run;
    callbacks.EvtAcxStreamPause = Pause;
    callbacks.EvtAcxStreamAssignDrmContentId = Drm;
    auto status = AcxStreamInitAssignAcxStreamCallbacks(init, &callbacks);
    ACX_RT_STREAM_CALLBACKS rt;
    ACX_RT_STREAM_CALLBACKS_INIT(&rt);
    rt.EvtAcxStreamGetHwLatency = Latency;
    rt.EvtAcxStreamAllocateRtPackets = Allocate;
    rt.EvtAcxStreamFreeRtPackets = Free;
    rt.EvtAcxStreamGetCurrentPacket = CurrentPacket;
    rt.EvtAcxStreamGetPresentationPosition = Position;
    rt.EvtAcxStreamSetRenderPacket = circuitCtx->Capture ? nullptr : RenderPacket;
    rt.EvtAcxStreamGetCapturePacket = circuitCtx->Capture ? CapturePacket : nullptr;
    if (NT_SUCCESS(status)) status = AcxStreamInitAssignAcxRtStreamCallbacks(init, &rt);
    AcxStreamInitSetAcxRtStreamSupportsNotifications(init);
    WDF_OBJECT_ATTRIBUTES attributes;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes, StreamContext);
    attributes.EvtCleanupCallback = StreamCleanup;
    attributes.ExecutionLevel = WdfExecutionLevelPassive;
    ACXSTREAM stream = nullptr;
    if (NT_SUCCESS(status)) status = AcxRtStreamCreate(device, circuit, &attributes, &init, &stream);
    if (!NT_SUCCESS(status)) { InterlockedExchange(&circuitCtx->Streams, 0); return status; }
    auto ctx = StreamData(stream);
    ctx->Device = device;
    ctx->Circuit = circuit;
    WdfObjectReference(circuit);
    ctx->Capture = circuitCtx->Capture;
    ctx->Claimed = TRUE;
    KeInitializeSpinLock(&ctx->Lock);
    LARGE_INTEGER frequency;
    KeQueryPerformanceCounter(&frequency);
    ctx->Frequency = frequency.QuadPart;
    WDF_TIMER_CONFIG timer;
    WDF_TIMER_CONFIG_INIT(&timer, Tick);
    timer.AutomaticSerialization = FALSE;
    timer.UseHighResolutionTimer = WdfTrue;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes, TimerContext);
    attributes.ParentObject = stream;
    attributes.ExecutionLevel = WdfExecutionLevelDispatch;
    attributes.EvtCleanupCallback = TimerCleanup;
    WDFTIMER handle = nullptr;
    status = WdfTimerCreate(&timer, &attributes, &handle);
    if (NT_SUCCESS(status)) {
        // WDF clears its callback-owner association during timer disposal,
        // before EvtCleanupCallback. Keep our own reference through cleanup.
        WdfObjectReference(stream);
        TimerData(handle)->Stream = stream;
        ctx->Timer = handle;
    } else WdfObjectDelete(stream);
    return status;
}

static void ReleasePackets(_In_reads_(count) _Post_invalid_ __drv_freesMem(Mem) PACX_RTPACKET packets, ULONG count) {
    for (ULONG i = 0; i < count; ++i) {
        auto mdl = packets[i].RtPacketBuffer.u.MdlType.Mdl;
        if (mdl) {
            auto buffer = MmGetMdlVirtualAddress(mdl);
            IoFreeMdl(mdl);
            ExFreePoolWithTag(buffer, OU_TAG);
        }
    }
    ExFreePoolWithTag(packets, OU_TAG);
}

static NTSTATUS Allocate(ACXSTREAM stream, ULONG count, ULONG bytes, PACX_RTPACKET* result) {
    *result = nullptr;
    auto ctx = StreamData(stream);
    if (ctx->Packets || ctx->Running || !count || count > 2 || bytes < 480 || bytes > 192000 ||
        bytes % 4 || (count == 1 && bytes % PAGE_SIZE)) return STATUS_INVALID_PARAMETER;
    auto packets = (PACX_RTPACKET)ExAllocatePool2(POOL_FLAG_NON_PAGED, sizeof(ACX_RTPACKET) * count, OU_TAG);
    if (!packets) return STATUS_INSUFFICIENT_RESOURCES;
    ULONG allocated = (bytes + PAGE_SIZE - 1) & ~(PAGE_SIZE - 1);
    PVOID buffers[2]{};
    for (ULONG i = 0; i < count; ++i) {
        ACX_RTPACKET_INIT(&packets[i]);
        auto buffer = ExAllocatePool2(POOL_FLAG_NON_PAGED, allocated, OU_TAG);
        if (!buffer) { ReleasePackets(packets, count); return STATUS_INSUFFICIENT_RESOURCES; }
        auto mdl = IoAllocateMdl(buffer, allocated, FALSE, FALSE, nullptr);
        if (!mdl) { ExFreePoolWithTag(buffer, OU_TAG); ReleasePackets(packets, count); return STATUS_INSUFFICIENT_RESOURCES; }
        MmBuildMdlForNonPagedPool(mdl);
        WDF_MEMORY_DESCRIPTOR_INIT_MDL(&packets[i].RtPacketBuffer, mdl, allocated);
        packets[i].RtPacketSize = bytes;
        packets[i].RtPacketOffset = count == 2 && i == 0 ? allocated - bytes : 0;
        buffers[i] = (PUCHAR)buffer + packets[i].RtPacketOffset;
    }
    // Publish only a complete allocation. Failures before this point belong
    // to this function, not to ACX's successful-allocation release callback.
    KIRQL irql;
    KeAcquireSpinLock(&ctx->Lock, &irql);
    ctx->Packets = packets;
    ctx->Count = count;
    ctx->Bytes = bytes;
    ctx->Frames = bytes / 4;
    RtlCopyMemory(ctx->Buffers, buffers, sizeof(buffers));
    KeReleaseSpinLock(&ctx->Lock, irql);
    *result = packets;
    return STATUS_SUCCESS;
}

static void Free(ACXSTREAM stream, PACX_RTPACKET packets, ULONG count) {
    auto ctx = StreamData(stream);
    if (packets != ctx->Packets || count != ctx->Count || !packets) {
        RecordFailure(3, __LINE__, STATUS_INVALID_DEVICE_STATE);
        return;
    }
    Pause(stream);
    // Detach before releasing storage. ACX owns the successful allocation's
    // release callback; object cleanup must never release the same packets.
    KIRQL irql;
    KeAcquireSpinLock(&ctx->Lock, &irql);
    ctx->Packets = nullptr;
    ctx->Buffers[0] = ctx->Buffers[1] = nullptr;
    ctx->Count = ctx->Bytes = ctx->Frames = 0;
    RtlZeroMemory(ctx->RenderValid, sizeof(ctx->RenderValid));
    KeReleaseSpinLock(&ctx->Lock, irql);
    ReleasePackets(packets, count);
}

static NTSTATUS PrepareStream(ACXSTREAM stream) {
    auto ctx = StreamData(stream);
    ctx->Prepared = TRUE;
    return STATUS_SUCCESS;
}
static NTSTATUS ReleaseStream(ACXSTREAM stream) {
    Pause(stream);
    auto ctx = StreamData(stream);
    KIRQL irql;
    KeAcquireSpinLock(&ctx->Lock, &irql);
    ctx->Prepared = FALSE;
    ctx->Position = ctx->AnchorPosition = 0;
    ctx->Current = 0;
    ctx->Eos = FALSE;
    RtlZeroMemory(ctx->RenderValid, sizeof(ctx->RenderValid));
    KeReleaseSpinLock(&ctx->Lock, irql);
    return STATUS_SUCCESS;
}
static NTSTATUS Run(ACXSTREAM stream) {
    auto ctx = StreamData(stream);
    if (!ctx->Prepared || !ctx->Packets || !ctx->Timer) return STATUS_INVALID_DEVICE_STATE;
    KIRQL irql;
    KeAcquireSpinLock(&ctx->Lock, &irql);
    if (ctx->Running) { KeReleaseSpinLock(&ctx->Lock, irql); return STATUS_SUCCESS; }
    ctx->AnchorPosition = ctx->Position;
    ctx->AnchorQpc = KeQueryPerformanceCounter(nullptr).QuadPart;
    ctx->Running = TRUE;
    KeReleaseSpinLock(&ctx->Lock, irql);
    PublishRunning(ctx, TRUE);
    KeAcquireSpinLock(&ctx->Lock, &irql);
    if (ctx->Running && ctx->Timer) Schedule(ctx);
    KeReleaseSpinLock(&ctx->Lock, irql);
    return STATUS_SUCCESS;
}
static NTSTATUS Pause(ACXSTREAM stream) {
    auto ctx = StreamData(stream);
    KIRQL irql;
    KeAcquireSpinLock(&ctx->Lock, &irql);
    BOOLEAN wasRunning = ctx->Running;
    ctx->Running = FALSE;
    KeReleaseSpinLock(&ctx->Lock, irql);
    if (ctx->Timer) WdfTimerStop(ctx->Timer, TRUE);
    if (wasRunning) {
        KeAcquireSpinLock(&ctx->Lock, &irql);
        for (ULONG i = 0; i < ctx->Count; ++i) {
            if (ctx->Buffers[i]) RtlZeroMemory(ctx->Buffers[i], ctx->Bytes);
        }
        RtlZeroMemory(ctx->RenderValid, sizeof(ctx->RenderValid));
        KeReleaseSpinLock(&ctx->Lock, irql);
        PublishRunning(ctx, FALSE);
    }
    return STATUS_SUCCESS;
}
static void StreamCleanup(WDFOBJECT object) {
    auto ctx = StreamData(object);
    if (!ctx->Claimed) return;
    Pause((ACXSTREAM)object);
    // ACX's post-cleanup callback releases WaveRT packets after this callback.
    // Keep the descriptor valid for that one owner (EvtAcxStreamFreeRtPackets).
    InterlockedExchange(&CircuitData(ctx->Circuit)->Streams, 0);
    ctx->Claimed = FALSE;
    WdfObjectDereference(ctx->Circuit);
}
static void TimerCleanup(WDFOBJECT object) {
    // WDF deletes children before the parent's cleanup callback. Quiesce the
    // DPC while this timer handle is still valid, then stop exposing it to
    // StreamCleanup / ACX's later FreeRtPackets callback. Timer cleanup runs
    // at PASSIVE_LEVEL even though Tick runs at DISPATCH_LEVEL.
    auto timer = TimerData(object);
    auto stream = timer->Stream;
    // A failed WdfTimerCreate can dispose its context before we acquire the
    // stream reference. In that case there is nothing for this callback to own.
    if (!stream) return;
    Pause(stream);
    StreamData(stream)->Timer = nullptr;
    timer->Stream = nullptr;
    WdfObjectDereference(stream);
}
static NTSTATUS Latency(ACXSTREAM, PULONG fifo, PULONG delay) { *fifo = *delay = 0; return STATUS_SUCCESS; }
static NTSTATUS CurrentPacket(ACXSTREAM stream, PULONG packet) {
    auto ctx = StreamData(stream);
    KIRQL irql;
    KeAcquireSpinLock(&ctx->Lock, &irql);
    *packet = ctx->Current;
    KeReleaseSpinLock(&ctx->Lock, irql);
    return STATUS_SUCCESS;
}
static NTSTATUS Position(ACXSTREAM stream, PULONGLONG position, PULONGLONG qpc) {
    auto ctx = StreamData(stream);
    KIRQL irql;
    KeAcquireSpinLock(&ctx->Lock, &irql);
    *qpc = KeQueryPerformanceCounter(nullptr).QuadPart;
    // Report only memory actually consumed/filled; timer-driven clients can
    // otherwise overwrite render PCM or read capture PCM before the DPC ran.
    *position = ctx->Position;
    KeReleaseSpinLock(&ctx->Lock, irql);
    return STATUS_SUCCESS;
}
static NTSTATUS RenderPacket(ACXSTREAM stream, ULONG packet, ULONG flags, ULONG eosBytes) {
    auto ctx = StreamData(stream);
    KIRQL irql;
    KeAcquireSpinLock(&ctx->Lock, &irql);
    NTSTATUS status = STATUS_SUCCESS;
    LONG ahead = (LONG)(packet - ctx->Current);
    if (flags & ~KSSTREAM_HEADER_OPTIONSF_ENDOFSTREAM ||
        (flags && (eosBytes > ctx->Bytes || eosBytes % 4))) status = STATUS_INVALID_PARAMETER;
    else if (!ctx->Count || ctx->Eos) status = STATUS_INVALID_DEVICE_STATE;
    else if (ahead < 0 || (ctx->Running && ahead == 0)) status = STATUS_DATA_LATE_ERROR;
    else if ((ULONG)ahead >= ctx->Count) status = STATUS_DATA_OVERRUN;
    else {
        ULONG index = packet % ctx->Count;
        ctx->RenderValid[index] = TRUE;
        ctx->RenderPacket[index] = packet;
        ctx->RenderBytes[index] = flags ? eosBytes : ctx->Bytes;
        ctx->Eos = flags != 0;
    }
    KeReleaseSpinLock(&ctx->Lock, irql);
    return status;
}
static NTSTATUS CapturePacket(ACXSTREAM stream, PULONG packet, PULONGLONG qpc, PBOOLEAN more) {
    auto ctx = StreamData(stream);
    KIRQL irql;
    KeAcquireSpinLock(&ctx->Lock, &irql);
    auto status = ctx->Position >= ctx->Frames && ctx->Frames ? STATUS_SUCCESS : STATUS_DEVICE_NOT_READY;
    *packet = ctx->Current - 1;
    *qpc = ctx->LastPacketQpc;
    *more = FALSE;
    KeReleaseSpinLock(&ctx->Lock, irql);
    return status;
}
static NTSTATUS Drm(ACXSTREAM stream, ULONG, PACXDRMRIGHTS rights) {
    auto ctx = StreamData(stream);
    KIRQL irql;
    KeAcquireSpinLock(&ctx->Lock, &irql);
    ctx->Protected = rights->CopyProtect || rights->DigitalOutputDisable;
    if (ctx->Protected) {
        auto device = DeviceContext(ctx->Device);
        KIRQL nested;
        KeAcquireSpinLock(&device->Lock, &nested);
        device->Speaker.Clear();
        KeReleaseSpinLock(&device->Lock, nested);
    }
    KeReleaseSpinLock(&ctx->Lock, irql);
    return STATUS_SUCCESS;
}

static void Tick(WDFTIMER timer) {
    auto stream = TimerData(timer)->Stream;
    if (!stream) return;
    auto ctx = StreamData(stream);
    auto device = DeviceContext(ctx->Device);
    KIRQL irql;
    KeAcquireSpinLock(&ctx->Lock, &irql);
    if (!ctx->Running || !ctx->Packets) { KeReleaseSpinLock(&ctx->Lock, irql); return; }
    ULONGLONG now = KeQueryPerformanceCounter(nullptr).QuadPart;
    ULONGLONG clock = ClockFrames(ctx, now);
    // A stalled CPU must not cause recursive catch-up, stale replay or a
    // backlog. Resume at the real clock, within at most one packet of it.
    if (clock > ctx->Position + ctx->Frames * 2ull) {
        ctx->Position = (clock / ctx->Frames) * ctx->Frames;
        ctx->Current = (ULONG)(ctx->Position / ctx->Frames);
        RtlZeroMemory(ctx->RenderValid, sizeof(ctx->RenderValid));
        KIRQL nested;
        KeAcquireSpinLock(&device->Lock, &nested);
        if (ctx->Capture) device->Microphone.Clear(); else device->Speaker.Clear();
        if (ctx->Capture) device->State.Generation++;
        KeReleaseSpinLock(&device->Lock, nested);
    }
    ULONG step = Step(ctx);
    if (ctx->Position + step > clock) {
        Schedule(ctx);
        KeReleaseSpinLock(&ctx->Lock, irql);
        return;
    }
    ULONG index = ctx->Current % ctx->Count;
    ULONG offset = (ULONG)(ctx->Position % ctx->Frames);
    auto data = (short*)ctx->Buffers[index] + offset * 2;
    auto controls = CircuitData(ctx->Circuit);
    ULONG gain[2];
    KIRQL controlsIrql;
    KeAcquireSpinLock(&controls->Controls, &controlsIrql);
    for (ULONG c = 0; c < 2; ++c) gain[c] = controls->Mute[c] ? 0 : controls->Gain[c];
    KeReleaseSpinLock(&controls->Controls, controlsIrql);
    KIRQL nested;
    KeAcquireSpinLock(&device->Lock, &nested);
    if (ctx->Capture) {
        ULONG read = (device->State.Flags & OU_AUDIO_MICROPHONE) ? device->Microphone.Read(data, step) : 0;
        RtlZeroMemory(data + read * 2, (step - read) * 4);
        if (device->State.Flags & OU_AUDIO_MICROPHONE) device->State.MicrophoneUnderrun += step - read;
        for (ULONG i = 0; i < step * 2; ++i) data[i] = (short)(((LONGLONG)data[i] * gain[i & 1]) / 65536);
    } else {
        bool valid = ctx->Count == 1 || (ctx->RenderValid[index] && ctx->RenderPacket[index] == ctx->Current);
        if (!valid || ctx->Protected) RtlZeroMemory(data, step * 4);
        else if (ctx->Count > 1 && ctx->RenderBytes[index] < ctx->Bytes)
            RtlZeroMemory((PUCHAR)ctx->Buffers[index] + ctx->RenderBytes[index], ctx->Bytes - ctx->RenderBytes[index]);
        if (device->State.Flags & OU_AUDIO_SPEAKER) {
            short adjusted[240];
            for (ULONG i = 0; i < step * 2; ++i) adjusted[i] = (short)(((LONGLONG)data[i] * gain[i & 1]) / 65536);
            device->State.SpeakerDropped += device->Speaker.Write(adjusted, step);
        }
        // A timer-driven client that misses its deadline must not replay data.
        if (ctx->Count == 1) RtlZeroMemory(data, step * 4);
    }
    device->State.Sequence++;
    KeReleaseSpinLock(&device->Lock, nested);
    ctx->Position += step;
    bool completed = ctx->Position % ctx->Frames == 0;
    ULONG packet = ctx->Current;
    if (completed) {
        ctx->RenderValid[index] = FALSE;
        ctx->Current++;
        ctx->LastPacketQpc = now - (ULONGLONG)ctx->Frames * ctx->Frequency / OU_AUDIO_RATE;
    }
    Schedule(ctx);
    KeReleaseSpinLock(&ctx->Lock, irql);
    if (completed) {
        auto status = AcxRtStreamNotifyPacketComplete(stream, packet, now);
        if (!NT_SUCCESS(status)) {
            KeAcquireSpinLock(&ctx->Lock, &irql);
            ctx->Running = FALSE;
            KeReleaseSpinLock(&ctx->Lock, irql);
            PublishRunning(ctx, FALSE);
        }
    }
    WakeBridge(device);
}

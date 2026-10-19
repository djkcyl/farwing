// ACX endpoint and streaming contracts follow Microsoft's ACX sample (LICENSE).
#pragma once
#include <ntddk.h>
#include <windef.h>
#define NOBITMAP
#include <mmreg.h>
#include <wdf.h>
#include <acx.h>
#include <ks.h>
#include <ksmedia.h>
#include "bridge.h"

#define OU_TAG 'aUPO'
#ifndef OU_UNIT
#define OU_UNIT 0
#endif
void RecordFailure(ULONG unit, ULONG line, NTSTATUS status);
#define OU_TRY(expr) do { NTSTATUS s_ = (expr); if (!NT_SUCCESS(s_)) { RecordFailure(OU_UNIT, __LINE__, s_); return s_; } } while (0)

struct Ring {
    short Samples[OU_AUDIO_CAPACITY * 2];
    ULONG Head, Size;
    void Clear() { Head = Size = 0; RtlZeroMemory(Samples, sizeof(Samples)); }
    ULONG Read(short* out, ULONG frames);
    ULONG Write(const short* input, ULONG frames);
};
struct AudioDevice {
    ACXCIRCUIT Render, Capture;
    BOOLEAN RenderAdded, CaptureAdded, Online;
    KSPIN_LOCK Lock;
    Ring Speaker, Microphone;
    OU_AUDIO_STATE State;
    WDFDEVICE Bridge;
    WDFQUEUE Waits;
};
WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(AudioDevice, DeviceContext)
struct CircuitContext {
    BOOLEAN Capture; LONG Streams;
    KSPIN_LOCK Controls;
    LONG Volume[2];
    ULONG Gain[2], Mute[2];
};
WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(CircuitContext, CircuitData)
struct ElementContext { ACXCIRCUIT Circuit; };
WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(ElementContext, ElementData)
struct BridgeContext { WDFDEVICE Parent; };
WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(BridgeContext, BridgeData)
struct BridgeFileContext { BOOLEAN PowerHeld, Active; };
WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(BridgeFileContext, BridgeFileData)
struct StreamContext {
    WDFDEVICE Device;
    ACXCIRCUIT Circuit;
    WDFTIMER Timer;
    KSPIN_LOCK Lock;
    BOOLEAN Capture, Running, Claimed, Prepared, Protected, Eos;
    PACX_RTPACKET Packets;
    PVOID Buffers[2];
    ULONG Count, Bytes, Frames, Current, RenderPacket[2], RenderBytes[2];
    BOOLEAN RenderValid[2];
    ULONGLONG Position, AnchorPosition, AnchorQpc, Frequency, LastPacketQpc;
};
WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(StreamContext, StreamData)
struct TimerContext { ACXSTREAM Stream; };
WDF_DECLARE_CONTEXT_TYPE_WITH_NAME(TimerContext, TimerData)

NTSTATUS CreateCircuit(WDFDEVICE device, BOOLEAN capture, ACXCIRCUIT* result);
NTSTATUS CreateBridge(WDFDEVICE device);
void WakeBridge(AudioDevice* context);
void ClearBridge(AudioDevice* context, bool revoke = true);
EVT_ACX_CIRCUIT_CREATE_STREAM CreateStream;
extern "C" DRIVER_INITIALIZE DriverEntry;

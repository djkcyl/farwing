// Based on Microsoft ACX sample circuit construction; see LICENSE/UPSTREAM.md.
#define OU_UNIT 2
#include <initguid.h>
#include "audio.h"
#include <limits.h>

static const GUID RenderId = {0xb6d42785,0x7971,0x42c9,{0x80,0x03,0xdd,0x38,0x04,0x02,0xc6,0x0b}};
static const GUID CaptureId = {0x9715079d,0xb3a3,0x47ba,{0xa7,0xc1,0x7d,0x77,0xbd,0x64,0x28,0x2a}};
static EVT_ACX_CIRCUIT_POWER_UP InitializeCircuit;
static EVT_ACX_RAMPED_VOLUME_ASSIGN_LEVEL SetVolume;
static EVT_ACX_VOLUME_RETRIEVE_LEVEL GetVolume;
static EVT_ACX_MUTE_ASSIGN_STATE SetMute;
static EVT_ACX_MUTE_RETRIEVE_STATE GetMute;

static const ULONG Gains[] = { 65536, 58409, 52057, 46396, 41350, 36854, 32846, 29274, 26090, 23253, 20724, 18471, 16462, 14672, 13076, 11654, 10387, 9257, 8250, 7353, 6554, 5841, 5206, 4640, 4135, 3685, 3285, 2927, 2609, 2325, 2072, 1847, 1646, 1467, 1308, 1165, 1039, 926, 825, 735, 655, 584, 521, 464, 414, 369, 328, 293, 261, 233, 207, 185, 165, 147, 131, 117, 104, 93, 83, 74, 66, 58, 52, 46, 41, 37, 33, 29, 26, 23, 21, 18, 16, 15, 13, 12, 10, 9, 8, 7, 7, 6, 5, 5, 4, 4, 3, 3, 3, 2, 2, 2, 2, 1, 1, 1, 1 };
static NTSTATUS SetVolume(ACXVOLUME element, ULONG channel, LONG level, ACX_VOLUME_CURVE_TYPE, ULONGLONG) {
    if ((channel != ULONG_MAX && channel >= 2) || level > 0 || level < -96 * 65536) return STATUS_INVALID_PARAMETER;
    auto ctx = CircuitData(ElementData(element)->Circuit);
    KIRQL irql; KeAcquireSpinLock(&ctx->Controls, &irql);
    for (ULONG i = 0; i < 2; ++i) if (channel == ULONG_MAX || channel == i) {
        ctx->Volume[i] = level;
        ctx->Gain[i] = Gains[(ULONG)(-level) / 65536];
    }
    KeReleaseSpinLock(&ctx->Controls, irql);
    return STATUS_SUCCESS;
}
static NTSTATUS GetVolume(ACXVOLUME element, ULONG channel, PLONG level) {
    if (channel != ULONG_MAX && channel >= 2) return STATUS_INVALID_PARAMETER;
    auto ctx = CircuitData(ElementData(element)->Circuit);
    KIRQL irql; KeAcquireSpinLock(&ctx->Controls, &irql);
    *level = ctx->Volume[channel == ULONG_MAX ? 0 : channel];
    KeReleaseSpinLock(&ctx->Controls, irql);
    return STATUS_SUCCESS;
}
static NTSTATUS SetMute(ACXMUTE element, ULONG channel, ULONG mute) {
    if ((channel != ULONG_MAX && channel >= 2) || mute > 1) return STATUS_INVALID_PARAMETER;
    auto ctx = CircuitData(ElementData(element)->Circuit);
    KIRQL irql; KeAcquireSpinLock(&ctx->Controls, &irql);
    for (ULONG i = 0; i < 2; ++i) if (channel == ULONG_MAX || channel == i) ctx->Mute[i] = mute;
    KeReleaseSpinLock(&ctx->Controls, irql);
    return STATUS_SUCCESS;
}
static NTSTATUS GetMute(ACXMUTE element, ULONG channel, PULONG mute) {
    if (channel != ULONG_MAX && channel >= 2) return STATUS_INVALID_PARAMETER;
    auto ctx = CircuitData(ElementData(element)->Circuit);
    KIRQL irql; KeAcquireSpinLock(&ctx->Controls, &irql);
    *mute = ctx->Mute[channel == ULONG_MAX ? 0 : channel];
    KeReleaseSpinLock(&ctx->Controls, irql);
    return STATUS_SUCCESS;
}

static NTSTATUS InitializeCircuit(WDFDEVICE device, ACXCIRCUIT circuit, WDF_POWER_DEVICE_STATE) {
    // Use the registered circuit reference string on both Windows 10 and 11.
    // AcxCircuitGetSymbolicLinkName requires ACX 1.1, absent on Windows 10's
    // in-box 1.0 runtime even though its KMDF version is already 1.31.
    UNICODE_STRING reference;
    RtlInitUnicodeString(&reference, CircuitData(circuit)->Capture ? L"FarwingMicrophone" : L"FarwingSpeaker");
    WDF_OBJECT_ATTRIBUTES attributes;
    WDF_OBJECT_ATTRIBUTES_INIT(&attributes);
    attributes.ParentObject = circuit;
    WDFSTRING string;
    OU_TRY(WdfStringCreate(nullptr, &attributes, &string));
    auto status = WdfDeviceRetrieveDeviceInterfaceString(device, &KSCATEGORY_AUDIO, &reference, string);
    if (!NT_SUCCESS(status)) {
        WdfObjectDelete(string);
        RecordFailure(2, __LINE__, status);
        return status;
    }
    UNICODE_STRING audio{};
    WdfStringGetUnicodeString(string, &audio);
    // 120 frames = 2.5 ms. This is a declared engine quantum, not a latency
    // measurement. The PCM bridge never waits for a larger packet to fill.
    struct {
        KSAUDIO_PACKETSIZE_CONSTRAINTS2 Base;
        KSAUDIO_PACKETSIZE_PROCESSINGMODE_CONSTRAINT Extra[1];
    } limits{};
    limits.Base.MinPacketPeriodInHns = 25000;
    limits.Base.PacketSizeFileAlignment = FILE_LONG_ALIGNMENT;
    limits.Base.MaxPacketSizeInBytes = 0;
    limits.Base.NumProcessingModeConstraints = 2;
    limits.Base.ProcessingModeConstraints[0] = {AUDIO_SIGNALPROCESSINGMODE_RAW, 120, 0};
    limits.Extra[0] = {AUDIO_SIGNALPROCESSINGMODE_DEFAULT, 120, 0};
    status = IoSetDeviceInterfacePropertyData(&audio, &DEVPKEY_KsAudio_PacketSize_Constraints2,
        0, 0, DEVPROP_TYPE_BINARY, sizeof(limits), &limits);
    WdfObjectDelete(string);
    return status;
}

NTSTATUS CreateCircuit(WDFDEVICE device, BOOLEAN capture, ACXCIRCUIT* result) {
    *result = nullptr;
    auto init = AcxCircuitInitAllocate(device);
    if (!init) return STATUS_INSUFFICIENT_RESOURCES;
    UNICODE_STRING renderName = RTL_CONSTANT_STRING(L"FarwingSpeaker");
    UNICODE_STRING captureName = RTL_CONSTANT_STRING(L"FarwingMicrophone");
    AcxCircuitInitSetComponentId(init, capture ? &CaptureId : &RenderId);
    AcxCircuitInitSetCircuitType(init, capture ? AcxCircuitTypeCapture : AcxCircuitTypeRender);
    auto status = AcxCircuitInitAssignName(init, capture ? &captureName : &renderName);
    if (NT_SUCCESS(status)) status = AcxCircuitInitAssignAcxCreateStreamCallback(init, CreateStream);
    // These are standalone endpoints. Composite callbacks require composite
    // bridge bindings and cause ACX to reject an otherwise valid topology.
    ACX_CIRCUIT_PNPPOWER_CALLBACKS callbacks;
    ACX_CIRCUIT_PNPPOWER_CALLBACKS_INIT(&callbacks);
    callbacks.EvtAcxCircuitPowerUp = InitializeCircuit;
    AcxCircuitInitSetAcxCircuitPnpPowerCallbacks(init, &callbacks);
    WDF_OBJECT_ATTRIBUTES attributes;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes, CircuitContext);
    ACXCIRCUIT circuit = nullptr;
    if (NT_SUCCESS(status)) status = AcxCircuitCreate(device, &attributes, &init, &circuit);
    if (init) AcxCircuitInitFree(init);
    if (!NT_SUCCESS(status)) { RecordFailure(2, __LINE__, status); return status; }
    CircuitData(circuit)->Capture = capture;
    KeInitializeSpinLock(&CircuitData(circuit)->Controls);
    CircuitData(circuit)->Gain[0] = CircuitData(circuit)->Gain[1] = 65536;
    ACX_VOLUME_CALLBACKS volumeCallbacks;
    ACX_VOLUME_CALLBACKS_INIT(&volumeCallbacks);
    volumeCallbacks.EvtAcxRampedVolumeAssignLevel = SetVolume;
    volumeCallbacks.EvtAcxVolumeRetrieveLevel = GetVolume;
    ACX_VOLUME_CONFIG volume;
    ACX_VOLUME_CONFIG_INIT(&volume);
    volume.ChannelsCount = 2;
    volume.Minimum = -96 * 65536;
    volume.Maximum = 0;
    volume.SteppingDelta = 65536;
    volume.Name = &KSAUDFNAME_VOLUME_CONTROL;
    volume.Callbacks = &volumeCallbacks;
    WDF_OBJECT_ATTRIBUTES_INIT_CONTEXT_TYPE(&attributes, ElementContext);
    attributes.ParentObject = circuit;
    ACXELEMENT elements[2];
    OU_TRY(AcxVolumeCreate(circuit, &attributes, &volume, (ACXVOLUME*)&elements[0]));
    ElementData(elements[0])->Circuit = circuit;
    ACX_MUTE_CALLBACKS muteCallbacks;
    ACX_MUTE_CALLBACKS_INIT(&muteCallbacks);
    muteCallbacks.EvtAcxMuteAssignState = SetMute;
    muteCallbacks.EvtAcxMuteRetrieveState = GetMute;
    ACX_MUTE_CONFIG mute;
    ACX_MUTE_CONFIG_INIT(&mute);
    mute.ChannelsCount = 2;
    mute.Name = &KSAUDFNAME_WAVE_MUTE;
    mute.Callbacks = &muteCallbacks;
    OU_TRY(AcxMuteCreate(circuit, &attributes, &mute, (ACXMUTE*)&elements[1]));
    ElementData(elements[1])->Circuit = circuit;
    OU_TRY(AcxCircuitAddElements(circuit, elements, 2));

    ACXPIN pins[2];
    ACX_PIN_CONFIG pin;
    ACX_PIN_CONFIG_INIT(&pin);
    pin.Type = capture ? AcxPinTypeSource : AcxPinTypeSink;
    pin.Communication = AcxPinCommunicationSink;
    pin.Category = &KSCATEGORY_AUDIO;
    WDF_OBJECT_ATTRIBUTES_INIT(&attributes);
    attributes.ParentObject = circuit;
    OU_TRY(AcxPinCreate(circuit, &attributes, &pin, &pins[0]));
    ACX_PIN_CONFIG_INIT(&pin);
    pin.Type = capture ? AcxPinTypeSink : AcxPinTypeSource;
    pin.Communication = AcxPinCommunicationNone;
    pin.Category = capture ? &KSNODETYPE_MICROPHONE : &KSNODETYPE_SPEAKER;
    OU_TRY(AcxPinCreate(circuit, &attributes, &pin, &pins[1]));

    ACX_JACK_CONFIG jackConfig;
    ACX_JACK_CONFIG_INIT(&jackConfig);
    jackConfig.Description.ChannelMapping = SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT;
    jackConfig.Description.ConnectionType = AcxConnTypeOtherDigital;
    jackConfig.Description.GeoLocation = AcxGeoLocNotApplicable;
    jackConfig.Description.GenLocation = AcxGenLocInternal;
    jackConfig.Description.PortConnection = AcxPortConnIntegratedDevice;
    ACXJACK jack;
    attributes.ParentObject = pins[1];
    OU_TRY(AcxJackCreate(pins[1], &attributes, &jackConfig, &jack));
    OU_TRY(AcxPinAddJacks(pins[1], &jack, 1));

    KSDATAFORMAT_WAVEFORMATEXTENSIBLE wave{};
    wave.DataFormat.FormatSize = sizeof(wave);
    wave.DataFormat.SampleSize = 4;
    wave.DataFormat.MajorFormat = KSDATAFORMAT_TYPE_AUDIO;
    wave.DataFormat.SubFormat = KSDATAFORMAT_SUBTYPE_PCM;
    wave.DataFormat.Specifier = KSDATAFORMAT_SPECIFIER_WAVEFORMATEX;
    wave.WaveFormatExt.Format = {WAVE_FORMAT_EXTENSIBLE, 2, OU_AUDIO_RATE, OU_AUDIO_RATE * 4, 4, 16, 22};
    wave.WaveFormatExt.Samples.wValidBitsPerSample = 16;
    wave.WaveFormatExt.dwChannelMask = SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT;
    wave.WaveFormatExt.SubFormat = KSDATAFORMAT_SUBTYPE_PCM;
    ACX_DATAFORMAT_CONFIG formatConfig;
    ACX_DATAFORMAT_CONFIG_INIT_KS(&formatConfig, &wave);
    attributes.ParentObject = circuit;
    ACXDATAFORMAT format;
    OU_TRY(AcxDataFormatCreate(device, &attributes, &formatConfig, &format));
    auto formats = AcxPinGetRawDataFormatList(pins[0]);
    OU_TRY(AcxDataFormatListAddDataFormat(formats, format));
    OU_TRY(AcxDataFormatListAssignDefaultDataFormat(formats, format));
    OU_TRY(AcxPinAssignModeDataFormatList(pins[0], &AUDIO_SIGNALPROCESSINGMODE_DEFAULT, formats));
    OU_TRY(AcxCircuitAddPins(circuit, pins, 2));
    *result = circuit;
    return STATUS_SUCCESS;
}

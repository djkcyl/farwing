// Farwing Audio bridge ABI. Fixed-width, pointer-free buffered requests.
#pragma once
#define OU_AUDIO_ABI 1u
#define OU_AUDIO_RATE 48000u
#define OU_AUDIO_CHANNELS 2u
#define OU_AUDIO_BLOCK 480u
#define OU_AUDIO_CAPACITY 960u
#define OU_AUDIO_SPEAKER 1u
#define OU_AUDIO_MICROPHONE 2u
#define OU_AUDIO_IOCTL(n) CTL_CODE(FILE_DEVICE_UNKNOWN, 0x900 + (n), METHOD_BUFFERED, FILE_READ_DATA | FILE_WRITE_DATA)
#define OU_AUDIO_STATUS OU_AUDIO_IOCTL(0)
#define OU_AUDIO_ENABLE OU_AUDIO_IOCTL(1)
#define OU_AUDIO_READ OU_AUDIO_IOCTL(2)
#define OU_AUDIO_WRITE OU_AUDIO_IOCTL(3)
#define OU_AUDIO_WAIT OU_AUDIO_IOCTL(4)

typedef struct {
    unsigned int Abi, Flags;
} OU_AUDIO_CONFIG;
typedef struct {
    unsigned int Abi, Flags, SpeakerRunning, MicrophoneRunning;
    unsigned int SpeakerFrames, MicrophoneFrames, Reserved[2];
    unsigned long long Sequence, Generation, SpeakerDropped, MicrophoneUnderrun;
} OU_AUDIO_STATE;
typedef struct {
    unsigned int Abi, Frames;
    unsigned long long Generation;
    short Samples[OU_AUDIO_BLOCK * OU_AUDIO_CHANNELS];
} OU_AUDIO_PCM;

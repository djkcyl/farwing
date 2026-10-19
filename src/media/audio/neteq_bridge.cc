#include <algorithm>
#include <cstddef>
#include <cstdint>
#include <memory>
#include <new>
#include <vector>

#include "api/audio/audio_frame.h"
#include "api/audio_codecs/audio_decoder_factory.h"
#include "api/make_ref_counted.h"
#include "api/neteq/neteq.h"
#include "modules/audio_coding/codecs/opus/audio_decoder_opus.h"
#include "modules/audio_coding/neteq/default_neteq_factory.h"
#include "system_wrappers/include/clock.h"

namespace {
class OpusFactory : public webrtc::AudioDecoderFactory {
 public:
  std::vector<webrtc::AudioCodecSpec> GetSupportedDecoders() override {
    return {{webrtc::SdpAudioFormat("opus", 48000, 2),
             webrtc::AudioCodecInfo(48000, 2, 64000)}};
  }
  bool IsSupportedDecoder(const webrtc::SdpAudioFormat& format) override {
    return format.name == "opus" && format.clockrate_hz == 48000 && format.num_channels == 2;
  }
  std::unique_ptr<webrtc::AudioDecoder> MakeAudioDecoder(
      const webrtc::SdpAudioFormat& format,
      absl::optional<webrtc::AudioCodecPairId>) override {
    if (!IsSupportedDecoder(format)) return nullptr;
    return std::make_unique<webrtc::AudioDecoderOpusImpl>(2, 48000);
  }
};

// Packets are queued on the network thread and inserted when the audio
// device pulls a block. NetEq timestamps arrivals with its clock, so the
// clock is moved back to each packet's real arrival while it is inserted;
// otherwise every packet would appear to arrive in 10-ms bursts and inflate
// the jitter estimate.
class ArrivalClock : public webrtc::Clock {
 public:
  webrtc::Timestamp CurrentTime() override {
    const auto now = real_->CurrentTime();
    return backdate_us_ > 0 ? now - webrtc::TimeDelta::Micros(backdate_us_) : now;
  }
  webrtc::NtpTime ConvertTimestampToNtpTime(webrtc::Timestamp timestamp) override {
    return real_->ConvertTimestampToNtpTime(timestamp);
  }
  int64_t backdate_us_ = 0;

 private:
  webrtc::Clock* real_ = webrtc::Clock::GetRealTimeClock();
};

struct Receiver {
  // Declared before `neteq`, which keeps a pointer to it.
  ArrivalClock clock;
  std::unique_ptr<webrtc::NetEq> neteq;
  webrtc::AudioFrame frame;
  Receiver() {
    webrtc::NetEq::Config config;
    config.sample_rate_hz = 48000;
    config.max_packets_in_buffer = 200;
    config.min_delay_ms = 0;
    config.enable_post_decode_vad = false;
    webrtc::DefaultNetEqFactory factory;
    neteq = factory.CreateNetEq(config, rtc::make_ref_counted<OpusFactory>(), &clock);
    if (!neteq || !neteq->RegisterPayloadType(111, webrtc::SdpAudioFormat(
        "opus", 48000, 2, {{"stereo", "1"}, {"useinbandfec", "1"}}))) {
      throw std::bad_alloc();
    }
  }
};
}  // namespace

extern "C" {
struct OuNetEqStats {
  uint64_t output_samples;
  uint64_t concealed_samples;
  uint64_t inserted_samples;
  uint64_t removed_samples;
  uint64_t discarded_packets;
  uint32_t buffer_ms;
  uint32_t target_ms;
};

void* ou_neteq_create() noexcept {
  try { return new Receiver; } catch (...) { return nullptr; }
}

void ou_neteq_destroy(void* receiver) noexcept {
  delete static_cast<Receiver*>(receiver);
}

int ou_neteq_insert(void* receiver, const uint8_t* payload, size_t length,
                   uint32_t timestamp, uint16_t sequence, uint32_t age_us) noexcept {
  if (!receiver || !payload || length == 0 || length > 65535) return -1;
  auto& state = *static_cast<Receiver*>(receiver);
  try {
    webrtc::RTPHeader header;
    header.payloadType = 111;
    header.timestamp = timestamp;
    header.sequenceNumber = sequence;
    header.ssrc = 1;
    state.clock.backdate_us_ = std::min<int64_t>(age_us, 1000000);
    const int result = state.neteq->InsertPacket(
        header, rtc::ArrayView<const uint8_t>(payload, length));
    state.clock.backdate_us_ = 0;
    return result;
  } catch (...) {
    state.clock.backdate_us_ = 0;
    return -1;
  }
}

int ou_neteq_set_minimum_delay(void* receiver, int delay_ms) noexcept {
  if (!receiver || delay_ms < 0 || delay_ms > 10000) return -1;
  try {
    return static_cast<Receiver*>(receiver)->neteq->SetMinimumDelay(delay_ms) ? 0 : 1;
  } catch (...) { return -1; }
}

int ou_neteq_audio(void* receiver, float* output, size_t length,
                  OuNetEqStats* result) noexcept {
  if (!receiver || !output || length != 960 || !result) return -1;
  std::fill(output, output + length, 0.0f);
  try {
    auto& state = *static_cast<Receiver*>(receiver);
    bool muted = false;
    if (state.neteq->GetAudio(&state.frame, &muted) != 0) return 1;
    const auto channels = state.frame.num_channels_;
    if (state.frame.sample_rate_hz_ != 48000 || state.frame.samples_per_channel_ != 480 ||
        (channels != 1 && channels != 2)) return -1;
    if (!muted) {
      const int16_t* pcm = state.frame.data();
      for (size_t i = 0; i < 480; ++i) {
        output[2 * i] = pcm[channels * i] / 32768.0f;
        output[2 * i + 1] = pcm[channels * i + (channels == 2 ? 1 : 0)] / 32768.0f;
      }
    }
    const auto stats = state.neteq->GetLifetimeStatistics();
    const auto operations = state.neteq->GetOperationsAndState();
    *result = {stats.total_samples_received, stats.concealed_samples,
               stats.inserted_samples_for_deceleration, stats.removed_samples_for_acceleration,
               stats.packets_discarded, static_cast<uint32_t>(operations.current_buffer_size_ms),
               static_cast<uint32_t>(state.neteq->TargetDelayMs())};
    return 0;
  } catch (...) { return -1; }
}
}

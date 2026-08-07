#include "math.h"
#include "miniaudio.h"
#include <SDL3/SDL.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define LOCAL_DEV_MIC "MacBook Pro Microphone"
#define LOCAL_DEV_SPEAKERS "MacBook Pro Speakers"
#define LOCAL_DEV_HEADPHONES "Hyper Nova"
#define AUDIO_INTERFACE "Scarlett 2i2 4th Gen"
#define SAMPLE_FILE "data/guitsample.mp3"

const bool NO_INTERFACE = true;

ma_device_id *find_capture_device_by_name(ma_device_info *infos,
                                          ma_uint32 count,
                                          const char *target_name) {
  for (ma_uint32 i = 0; i < count; i++) {
    if (strcmp(infos[i].name, target_name) == 0) {
      return &infos[i].id;
    }
  }
  return NULL;
}

typedef enum {
  FX_NONE = 0,
  FX_DISTORTION = 1 << 0, //  0001
  FX_RECTIFIED = 1 << 2   //  0010
} effect_flags;

float distort(float sample, float drive) { return tanhf(sample * drive); }
float rectify(float sample) { return fabsf(sample); }
void data_callback(ma_device *device, void *output, const void *input,
                   ma_uint32 frame_count) {
  const float *in = (const float *)input;
  float *out = (float *)output;

  for (ma_uint32 i = 0; i < frame_count * device->capture.channels; i++) {
    // TODO: fix feedback on live monitors
    // out[i] = distort(in[i], 2.0f);
    out[i] = i[in];
    // printf("data: [%d, %f]\n", i, in[i]);
  }
}

void capture_audio() {
  ma_device_info *playback_infos;
  ma_uint32 playback_count;
  ma_device_info *capture_infos;
  ma_uint32 capture_count = 0;
  ma_context context;

  if (ma_context_init(NULL, 0, NULL, &context) != MA_SUCCESS) {
    fprintf(stderr, "failed to init context\n");
    return;
  }

  if (ma_context_get_devices(&context, &playback_infos, &playback_count,
                             &capture_infos, &capture_count) != MA_SUCCESS) {
    fprintf(stderr, "failed to enumerate devices\n");
    return;
  }

  for (ma_uint32 i = 0; i < capture_count; i++) {
    printf("capture device %d: %s\n", i, capture_infos[i].name);
  }
  for (ma_uint32 i = 0; i < playback_count; i++) {
    printf("playback device %d: %s\n", i, playback_infos[i].name);
  }

  ma_device_id *capture_id = find_capture_device_by_name(
      capture_infos, capture_count,
      NO_INTERFACE ? LOCAL_DEV_MIC : AUDIO_INTERFACE);

  ma_device_id *playback_id = find_capture_device_by_name(
      capture_infos, capture_count,
      NO_INTERFACE ? LOCAL_DEV_HEADPHONES : AUDIO_INTERFACE);

  ma_device_config config = ma_device_config_init(ma_device_type_duplex);
  config.sampleRate = 48000;

  config.capture.pDeviceID = capture_id;
  config.capture.format = ma_format_f32;
  config.capture.channels = 2;

  config.playback.pDeviceID = playback_id;
  config.playback.format = ma_format_f32;
  config.playback.channels = 2;
  config.dataCallback = data_callback;

  config.periodSizeInFrames =
      128;            // smaller = lower latency, more underrun risk
  config.periods = 2; // fewer eriods = lower latency, less safety margin
  config.performanceProfile =
      ma_performance_profile_low_latency; // hints backend to prefer smaller
                                          // buffers

  ma_device device;
  if (ma_device_init(NULL, &config, &device) != MA_SUCCESS) {
    fprintf(stderr, "failed to init duplex device\n");
    return;
  }

  ma_device_start(&device);

  printf("pausing for input\n");
  sleep(100);
  ma_device_uninit(&device);

  ma_context_uninit(&context);
}

typedef struct {
  float *data;
  ma_uint64 total_frames;
  ma_uint64 read_cursor;
  ma_uint32 channels;
} sample_buffer;

typedef struct {
  effect_flags effect_flags;
  union {
    struct {
      float drive;
    } distortion;
  } params;
} effect_state;

typedef struct {
  sample_buffer sb;
  effect_state fx;
} playback_ctx;

static float apply_effect(effect_state *fx, float sample) {
  float processed = sample;
  if (fx->effect_flags & FX_DISTORTION) {
    processed = distort(sample, fx->params.distortion.drive);
  }
  if (fx->effect_flags & FX_RECTIFIED) {
    processed = rectify(processed);
  }
  return processed;
}

int count = 0;
int subcount = 0;
void playback_callback(ma_device *device, void *output, const void *input,
                       ma_uint32 frame_count) {
  playback_ctx *ctx = (playback_ctx *)device->pUserData;
  sample_buffer *sb = &ctx->sb;
  float *out = (float *)output;
  count += 1;
  printf("count %d\n", count);
  for (ma_uint32 i = 0; i < frame_count; i++) {
    if (sb->read_cursor >= sb->total_frames) {
      sb->read_cursor = 0; // loop
    }
    for (ma_uint32 c = 0; c < sb->channels; c++) {
      float sample = sb->data[sb->read_cursor * sb->channels + c];
      float processed = apply_effect(&ctx->fx, sample);
      out[i * sb->channels + c] = processed;
      subcount += 1;
      printf("subcount: %d\n", subcount);
    }
    sb->read_cursor++;
  }
}

int load_sample(const char *path, sample_buffer *sb) {
  ma_decoder decoder;
  ma_decoder_config decoder_config = ma_decoder_config_init(
      ma_format_f32, 1,
      48000); // force format/channels/rate; force mono for now

  if (ma_decoder_init_file(path, &decoder_config, &decoder) != MA_SUCCESS) {
    fprintf(stderr, "failed to load %s\n", path);
    return -1;
  }

  ma_uint64 total_frames;
  ma_decoder_get_length_in_pcm_frames(&decoder, &total_frames);

  sb->channels = decoder_config.channels;
  sb->total_frames = total_frames;
  sb->data = (float *)malloc(total_frames * sb->channels * sizeof(float));
  sb->read_cursor = 0;

  ma_uint64 frames_read;
  ma_decoder_read_pcm_frames(&decoder, sb->data, total_frames, &frames_read);

  ma_decoder_uninit(&decoder);

  printf("loaded %llu frames from %s\n", (unsigned long long)frames_read, path);
  return 0;
}

void playback_sample(const char *device_name, playback_ctx *ctx) {
  ma_context context;
  ma_context_init(NULL, 0, NULL, &context);

  ma_device_info *capture_infos;
  ma_uint32 capture_count;
  ma_device_info *playback_infos;
  ma_uint32 playback_count;
  ma_context_get_devices(&context, &playback_infos, &playback_count,
                         &capture_infos, &capture_count);

  ma_device_id *playback_id =
      find_capture_device_by_name(playback_infos, playback_count, device_name);

  ctx->sb.read_cursor = 0;

  ma_device_config config = ma_device_config_init(ma_device_type_playback);
  config.sampleRate = 48000;
  config.playback.pDeviceID = playback_id;
  config.playback.format = ma_format_f32;
  config.playback.channels = ctx->sb.channels;
  config.dataCallback = playback_callback;
  config.pUserData = ctx;
  config.periodSizeInFrames = 128;

  ma_device device;
  ma_device_init(&context, &config, &device);
  ma_device_start(&device);

  printf("playing back...\n");
  sleep(8);

  ma_device_uninit(&device);
  ma_context_uninit(&context);
}

int play_sample(void) {
  playback_ctx ctx = {.sb = {0}, .fx = {0}};
  ctx.fx =
      (effect_state){.effect_flags = FX_DISTORTION | FX_RECTIFIED, .params.distortion.drive = 90.0f};

  if (load_sample(SAMPLE_FILE, &ctx.sb) != 0) {
    return 1;
  }

  playback_sample(NO_INTERFACE ? LOCAL_DEV_SPEAKERS : AUDIO_INTERFACE, &ctx);
  free(ctx.sb.data);
  return 0;
}

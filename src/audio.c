#include <SDL3/SDL.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define LOCAL_DEV_MIC "MacBook Pro Microphone"
#define LOCAL_DEV_HEADPHONES "Hyper Nova"

SDL_AudioDeviceID find_device_by_name(const char *target_name, bool recording) {
  int count = 0;
  SDL_AudioDeviceID *devices = recording ? SDL_GetAudioRecordingDevices(&count)
                                         : SDL_GetAudioPlaybackDevices(&count);
  SDL_AudioDeviceID device_id = 0;
  for (int i = 0; i < count; i++) {
    const char *name = SDL_GetAudioDeviceName(devices[i]);
    printf("checking device: %s\n", name);
    if (strcmp(name, target_name) == 0) {
      printf("found device %s\n", name);
      device_id = devices[i];
    }
  }
  SDL_free(devices);
  return device_id; // check for 0 in caller
}

void audio_callback(void *userdata, SDL_AudioStream *stream,
                    int additional_amount, int total_amount) {
  if (additional_amount > 0) {
    Uint8 *buf = (Uint8 *)SDL_malloc(additional_amount);
    if (buf) {
      int total_read = SDL_GetAudioStreamData(stream, buf, additional_amount);
      if (total_read > 0) {
        printf("grabbed %d bytes of audio\n", total_read);
      }
    }
    SDL_free(buf);
  }
};

void capture_audio() {
  SDL_AudioSpec spec = {0};
  ;
  spec.format = SDL_AUDIO_F32;
  spec.channels = 2;
  spec.freq = 48000;
  SDL_Init(SDL_INIT_AUDIO);

  SDL_AudioDeviceID record_devid = find_device_by_name(LOCAL_DEV_MIC, true);
  SDL_AudioStream *stream =
      SDL_OpenAudioDeviceStream(record_devid, &spec, NULL, NULL);

  if (!stream) {
    printf("error: %s\n", SDL_GetError());
    return;
  }

  SDL_ResumeAudioStreamDevice(stream);

  int bytes_per_sample = 4;
  int target_bytes = spec.freq * spec.channels * bytes_per_sample * 3;

  uint8_t *buffer = (uint8_t *)malloc(target_bytes);
  int total_read = 0;

  while (total_read < target_bytes) {
    int available = SDL_GetAudioStreamAvailable(stream);
    if (available > 0) {
      int to_read = available;
      if (total_read + to_read > target_bytes) {
        to_read = target_bytes - total_read;
      }
      int bytes_read =
          SDL_GetAudioStreamData(stream, buffer + total_read, to_read);
      if (bytes_read > 0) {
        total_read += bytes_read;
      }
    }
    SDL_Delay(1);
  }
  printf("break");

  SDL_AudioDeviceID playback_devid =
      find_device_by_name(LOCAL_DEV_HEADPHONES, false);

  SDL_AudioStream *playback_stream =
      SDL_OpenAudioDeviceStream(playback_devid, &spec, NULL, NULL);
  if (!playback_stream) {
    printf("error opening playback stream: %s\n", SDL_GetError());
    free(buffer);
    SDL_DestroyAudioStream(stream);
    return;
  }
  SDL_PutAudioStreamData(playback_stream, buffer, total_read);
  SDL_ResumeAudioStreamDevice(playback_stream);

  while (SDL_GetAudioStreamAvailable(playback_stream) > 0) {
    SDL_Delay(10);
  }

  const char *err = SDL_GetError();
  SDL_DestroyAudioStream(stream);
  SDL_DestroyAudioStream(playback_stream);
  free(buffer);
  printf("break");
}

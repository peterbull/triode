#include <SDL3/SDL.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define LOCAL_DEV_MIC "MacBook Pro Microphone"

SDL_AudioDeviceID find_device_by_name(const char *target_name, bool recording) {
  int count = 0;
  SDL_AudioDeviceID *devices = SDL_GetAudioRecordingDevices(&count);
  SDL_AudioDeviceID device_id = 0;
  for (int i = 0; i < count; i++) {
    const char *name = SDL_GetAudioDeviceName(devices[i]);
    if (strcmp(name, target_name) == 0) {
      printf("found device %s\n", name);
      device_id = devices[i]; 
      return device_id;
    }
  }
  return device_id; // check for 0 in caller
}

void capture_audio() {
  SDL_AudioSpec spec;
  spec.format = SDL_AUDIO_F32;
  spec.channels = 2;
  spec.freq = 48000;
  SDL_Init(SDL_INIT_AUDIO);

  SDL_AudioDeviceID device_id = find_device_by_name(LOCAL_DEV_MIC, false);
  SDL_AudioStream *stream = SDL_OpenAudioDeviceStream(
      device_id, &spec, NULL, NULL);
  
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
  printf("grabbed %d bytes of audio", total_read);
  free(buffer);
  const char *err = SDL_GetError();
  SDL_DestroyAudioStream(stream);

  printf("break");
}

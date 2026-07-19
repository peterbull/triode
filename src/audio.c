#include <SDL3/SDL.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

void capture_audio() {
  SDL_AudioSpec spec;
  spec.format = SDL_AUDIO_F32;
  spec.channels = 2;
  spec.freq = 48000;
  SDL_Init(SDL_INIT_AUDIO);
  SDL_AudioStream *stream = SDL_OpenAudioDeviceStream(
      SDL_AUDIO_DEVICE_DEFAULT_RECORDING, &spec, NULL, NULL);
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

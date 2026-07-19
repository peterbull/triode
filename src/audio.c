#include <SDL3/SDL.h>
#include <stdint.h>
#include <stdlib.h>

void capture_audio() {
  SDL_AudioSpec spec;
  spec.format = SDL_AUDIO_F32;
  spec.channels = 2;
  spec.freq = 48000;
  SDL_Init(SDL_INIT_AUDIO);  
  SDL_AudioStream *stream = SDL_OpenAudioDeviceStream(
      SDL_AUDIO_DEVICE_DEFAULT_RECORDING, &spec, NULL, NULL);
  const char* err = SDL_GetError(); 
  printf("error: %s\n", err);
  SDL_ResumeAudioStreamDevice(stream);
  
  int available_bytes = SDL_GetAudioStreamAvailable(stream); 
  if (available_bytes > 0) {
    uint8_t *buffer = (uint8_t *)malloc(available_bytes);
    int bytes_read = SDL_GetAudioStreamData(stream, buffer, available_bytes);
    if (bytes_read > 0) {
      printf("read some bytes");
    }
    free(buffer);
  }
  SDL_DestroyAudioStream(stream);
}

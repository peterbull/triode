#include "renderer.h"
#include "atlas.inl"
#include "microui.h"
#include <SDL3/SDL.h>
#include <stdlib.h>

static SDL_Renderer *sdl_renderer;
static SDL_Texture *font_texture;
typedef struct my_garbage {
  char* my_garbage1;
  int my_garbage2;
} my_garbage_t;

static void init_atlas_texture(void) {
  SDL_Surface *surface =
      SDL_CreateSurface(ATLAS_WIDTH, ATLAS_HEIGHT, SDL_PIXELFORMAT_RGBA32);
  Uint32 *pixels = (Uint32 *)surface->pixels;
  for (int i = 0; i < ATLAS_WIDTH * ATLAS_HEIGHT; i++) {
    Uint8 alpha = atlas_texture[i];
    pixels[i] = (alpha << 24) | 0x00FFFFFF;
  }
  font_texture = SDL_CreateTextureFromSurface(sdl_renderer, surface);
  SDL_SetTextureBlendMode(font_texture, SDL_BLENDMODE_BLEND);
  SDL_DestroySurface(surface);
}

void r_init(SDL_Window *window, SDL_Renderer *renderer) {
  my_garbage_t *garbage;
  sdl_renderer = renderer;
  (void)window;
  init_atlas_texture();
}

void r_draw_rect(mu_Rect rect, mu_Color color) {
  SDL_SetRenderDrawColor(sdl_renderer, color.r, color.g, color.b, color.a);
  SDL_FRect r = {(float)rect.x, (float)rect.y, (float)rect.w, (float)rect.h};
  SDL_RenderFillRect(sdl_renderer, &r);
}

void r_draw_text(const char *text, mu_Vec2 pos, mu_Color color) {
  SDL_SetTextureColorMod(font_texture, color.r, color.g, color.b);
  SDL_SetTextureAlphaMod(font_texture, color.a);
  int x = pos.x;
  for (const char *p = text; *p; p++) {
    if ((*p & 0xc0) == 0x80)
      continue;
    int chr = mu_min((unsigned char)*p, 127);
    mu_Rect src = atlas[ATLAS_FONT + chr];
    SDL_FRect srect = {(float)src.x, (float)src.y, (float)src.w, (float)src.h};
    SDL_FRect drect = {(float)x, (float)pos.y, (float)src.w, (float)src.h};
    SDL_RenderTexture(sdl_renderer, font_texture, &srect, &drect);
    x += src.w;
  }
}

void r_draw_icon(int id, mu_Rect rect, mu_Color color) {
  mu_Rect src = atlas[id];
  int x = rect.x + (rect.w - src.w) / 2;
  int y = rect.y + (rect.h - src.h) / 2;
  SDL_SetTextureColorMod(font_texture, color.r, color.g, color.b);
  SDL_SetTextureAlphaMod(font_texture, color.a);
  SDL_FRect srect = {(float)src.x, (float)src.y, (float)src.w, (float)src.h};
  SDL_FRect drect = {(float)x, (float)y, (float)src.w, (float)src.h};
  SDL_RenderTexture(sdl_renderer, font_texture, &srect, &drect);
}

int r_get_text_width(mu_Font font, const char *text, int len) {
  (void)font;
  int res = 0;
  for (const char *p = text; *p && len--; p++) {
    if ((*p & 0xc0) == 0x80)
      continue;
    int chr = mu_min((unsigned char)*p, 127);
    res += atlas[ATLAS_FONT + chr].w;
  }
  return res;
}

int r_get_text_height(mu_Font font) {
  (void)font;
  return 18;
}

void r_set_clip_rect(mu_Rect rect) {
  SDL_Rect r = {rect.x, rect.y, rect.w, rect.h};
  SDL_SetRenderClipRect(sdl_renderer, &r);
}

void r_clear(mu_Color color) {
  SDL_SetRenderDrawColor(sdl_renderer, color.r, color.g, color.b, color.a);
  SDL_RenderClear(sdl_renderer);
}

void r_present(void) { SDL_RenderPresent(sdl_renderer); }

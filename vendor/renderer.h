#ifndef RENDERER_H
#define RENDERER_H

#include "microui.h"
#include <SDL3/SDL.h>

void r_init(SDL_Window *window, SDL_Renderer *renderer);
void r_draw_rect(mu_Rect rect, mu_Color color);
void r_draw_text(const char *text, mu_Vec2 pos, mu_Color color);
void r_draw_icon(int id, mu_Rect rect, mu_Color color);
int r_get_text_height(void);
int r_get_text_width(mu_Font font, const char *text, int len);
void r_set_clip_rect(mu_Rect rect);
void r_clear(mu_Color color);
void r_present(void);

#endif

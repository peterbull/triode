#include "microui.h"
#include "renderer.h"
#include <SDL3/SDL.h>
#include <stdlib.h>

static const char button_map[256] = {
    [SDL_BUTTON_LEFT & 0xff] = MU_MOUSE_LEFT,
    [SDL_BUTTON_RIGHT & 0xff] = MU_MOUSE_RIGHT,
    [SDL_BUTTON_MIDDLE & 0xff] = MU_MOUSE_MIDDLE,
};

static void process_frame(mu_Context *ctx) {
  mu_begin(ctx);
  if (mu_begin_window(ctx, "Triode", mu_rect(50, 50, 300, 200))) {
    mu_layout_row(ctx, 1, (int[]){-1}, 0);
    mu_label(ctx, "Hello, Triode!");

    mu_layout_row(ctx, 1, (int[]){-1}, 0);
    if (mu_button(ctx, "Click me")) {
      SDL_Log("Button clicked!");
    }
    mu_end_window(ctx);
  }
  mu_end(ctx);
}

int main(void) {
  SDL_Init(SDL_INIT_VIDEO);

  SDL_Window *window =
      SDL_CreateWindow("Triode", 800, 600, SDL_WINDOW_RESIZABLE);
  SDL_Renderer *renderer = SDL_CreateRenderer(window, NULL);

  r_init(window, renderer);

  mu_Context *ctx = malloc(sizeof(mu_Context));
  mu_init(ctx);
  ctx->text_width = r_get_text_width;
  ctx->text_height = r_get_text_height;

  int running = 1;
  while (running) {
    SDL_Event evt;
    while (SDL_PollEvent(&evt)) {
      switch (evt.type) {
      case SDL_EVENT_QUIT:
        running = 0;
        break;
      case SDL_EVENT_MOUSE_MOTION:
        mu_input_mousemove(ctx, (int)evt.motion.x, (int)evt.motion.y);
        break;
      case SDL_EVENT_MOUSE_BUTTON_DOWN:
        mu_input_mousedown(ctx, (int)evt.button.x, (int)evt.button.y,
                           button_map[evt.button.button & 0xff]);
        break;
      case SDL_EVENT_MOUSE_BUTTON_UP:
        mu_input_mouseup(ctx, (int)evt.button.x, (int)evt.button.y,
                         button_map[evt.button.button & 0xff]);
        break;
      case SDL_EVENT_MOUSE_WHEEL:
        mu_input_scroll(ctx, 0, (int)(evt.wheel.y * -30));
        break;
      case SDL_EVENT_TEXT_INPUT:
        mu_input_text(ctx, evt.text.text);
        break;
      }
    }

    process_frame(ctx);

    r_clear(mu_color(30, 30, 30, 255));

    mu_Command *cmd = NULL;
    while (mu_next_command(ctx, &cmd)) {
      switch (cmd->type) {
      case MU_COMMAND_TEXT:
        r_draw_text(cmd->text.str, cmd->text.pos, cmd->text.color);
        break;
      case MU_COMMAND_RECT:
        r_draw_rect(cmd->rect.rect, cmd->rect.color);
        break;
      case MU_COMMAND_ICON:
        r_draw_icon(cmd->icon.id, cmd->icon.rect, cmd->icon.color);
        break;
      case MU_COMMAND_CLIP:
        r_set_clip_rect(cmd->clip.rect);
        break;
      }
    }

    r_present();
  }

  free(ctx);
  SDL_DestroyRenderer(renderer);
  SDL_DestroyWindow(window);
  SDL_Quit();
  return 0;
}

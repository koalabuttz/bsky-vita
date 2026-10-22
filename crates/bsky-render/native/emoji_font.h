#ifndef BSKY_EMOJI_FONT_H
#define BSKY_EMOJI_FONT_H
#include <stdint.h>
typedef struct emoji_font emoji_font;
typedef struct { uint32_t id; int32_t x_advance, y_advance, x_offset, y_offset; } emoji_glyph;
emoji_font *emoji_font_open(const char *path);
void emoji_font_close(emoji_font *font);
uint32_t emoji_font_strike_ppem(const emoji_font *font);
int emoji_font_shape(emoji_font *font, const uint8_t *text, uint32_t len, emoji_glyph *out, uint32_t capacity);
int emoji_font_render(emoji_font *font, uint32_t glyph, uint32_t *width, uint32_t *rows, int32_t *left, int32_t *top, uint8_t *pixels, uint32_t capacity);
#endif

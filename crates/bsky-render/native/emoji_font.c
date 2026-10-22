#include "emoji_font.h"
#include <ft2build.h>
#include FT_FREETYPE_H
#include FT_COLOR_H
#include <hb.h>
#include <hb-ft.h>
#include <stdlib.h>
#include <limits.h>
#include <string.h>

#define MAX_DIMENSION 256
#define MAX_PIXELS (MAX_DIMENSION * MAX_DIMENSION)
#define MAX_SCRATCH (256 * 1024)
#define MAX_INPUT 4096
#define MAX_GLYPHS 512

struct emoji_font {
    FT_Library library;
    FT_Face face;
    hb_font_t *font;
    hb_buffer_t *buffer;
    uint32_t strike_ppem;
};

emoji_font *emoji_font_open(const char *path) {
    emoji_font *f = calloc(1, sizeof(*f));
    if (!f) return NULL;
    if (FT_Init_FreeType(&f->library)) goto fail;
    if (FT_New_Face(f->library, path, 0, &f->face)) goto fail;
    if (!FT_HAS_COLOR(f->face) || f->face->num_fixed_sizes < 1 || f->face->num_fixed_sizes > 32) goto fail;
    int selected = -1;
    for (int i = 0; i < f->face->num_fixed_sizes; ++i) {
        FT_Bitmap_Size *s = &f->face->available_sizes[i];
        if (s->width <= 0 || s->height <= 0 || s->width > MAX_DIMENSION || s->height > MAX_DIMENSION || s->size <= 0 || s->size > MAX_DIMENSION * 64) goto fail;
        if (selected < 0) selected = i;
    }
    if (selected < 0 || FT_Select_Size(f->face, selected)) goto fail;
    f->strike_ppem = (uint32_t)((f->face->available_sizes[selected].y_ppem + 32) / 64);
    if (!f->strike_ppem || f->strike_ppem > MAX_DIMENSION) goto fail;
    f->font = hb_ft_font_create_referenced(f->face);
    if (!f->font) goto fail;
    f->buffer = hb_buffer_create();
    if (!f->buffer || !hb_buffer_allocation_successful(f->buffer)) goto fail;
    hb_ft_font_set_load_flags(f->font, FT_LOAD_COLOR);
    return f;
fail:
    emoji_font_close(f);
    return NULL;
}

void emoji_font_close(emoji_font *f) {
    if (!f) return;
    if (f->buffer) hb_buffer_destroy(f->buffer);
    if (f->font) hb_font_destroy(f->font);
    if (f->face) FT_Done_Face(f->face);
    if (f->library) FT_Done_FreeType(f->library);
    free(f);
}

uint32_t emoji_font_strike_ppem(const emoji_font *f) { return f ? f->strike_ppem : 0; }

int emoji_font_shape(emoji_font *f, const uint8_t *text, uint32_t len, emoji_glyph *out, uint32_t cap) {
    if (!f || !text || !len || len > MAX_INPUT || !out || !cap || cap > MAX_GLYPHS || (size_t)cap * sizeof(*out) > MAX_SCRATCH) return -1;
    hb_buffer_reset(f->buffer);
    hb_buffer_add_utf8(f->buffer, (const char *)text, (int)len, 0, (int)len);
    if (!hb_buffer_allocation_successful(f->buffer)) return -1;
    hb_buffer_set_flags(f->buffer, HB_BUFFER_FLAG_REMOVE_DEFAULT_IGNORABLES);
    hb_buffer_guess_segment_properties(f->buffer);
    hb_shape(f->font, f->buffer, NULL, 0);
    unsigned count = 0;
    hb_glyph_info_t *info = hb_buffer_get_glyph_infos(f->buffer, &count);
    hb_glyph_position_t *pos = hb_buffer_get_glyph_positions(f->buffer, &count);
    if (!info || !pos || !hb_buffer_allocation_successful(f->buffer)) return -1;
    if (count > cap) return -3;
    if (count != 1 || !info[0].codepoint || pos[0].x_advance <= 0) return -2;
    out[0] = (emoji_glyph){info[0].codepoint, pos[0].x_advance, pos[0].y_advance, pos[0].x_offset, pos[0].y_offset};
    return 1;
}

int emoji_font_render(emoji_font *f, uint32_t glyph, uint32_t *width, uint32_t *rows, int32_t *left, int32_t *top, uint8_t *pixels, uint32_t capacity) {
    if (!f || !width || !rows || !left || !top || capacity > MAX_SCRATCH || (capacity && !pixels)) return -1;
    if (FT_Load_Glyph(f->face, glyph, FT_LOAD_COLOR | FT_LOAD_RENDER)) return -1;
    FT_GlyphSlot slot = f->face->glyph;
    FT_Bitmap *b = &slot->bitmap;
    if (b->pixel_mode != FT_PIXEL_MODE_BGRA || b->width > MAX_DIMENSION || b->rows > MAX_DIMENSION || (b->width && b->rows && !b->buffer)) return -1;
    size_t row_bytes = (size_t)b->width * 4;
    size_t stride = (size_t)(b->pitch < 0 ? -(int64_t)b->pitch : b->pitch);
    if (stride < row_bytes || (b->rows && stride > MAX_SCRATCH / b->rows) || row_bytes * b->rows > capacity) return -1;
    *width = b->width; *rows = b->rows; *left = slot->bitmap_left; *top = slot->bitmap_top;
    if (!row_bytes || !b->rows) return 0;
    for (uint32_t y = 0; y < b->rows; ++y) {
        const uint8_t *src = b->buffer + (ptrdiff_t)y * b->pitch;
        memcpy(pixels + (size_t)y * row_bytes, src, row_bytes);
    }
    return 0;
}

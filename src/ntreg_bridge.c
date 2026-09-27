#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "ntreg.h"

typedef int (*bls_visit)(void *context, const char *path, const char *name,
                         int type, const unsigned char *data, int length);

static uint32_t read_u32_le(const unsigned char *bytes) {
    return (uint32_t)bytes[0] | ((uint32_t)bytes[1] << 8) |
           ((uint32_t)bytes[2] << 16) | ((uint32_t)bytes[3] << 24);
}

/* ntreg assumes several hive offsets are valid while opening the file. Check
 * its root cell before passing an untrusted Windows hive to that parser. */
static int has_valid_root_cell(const char *filename) {
    unsigned char header[0x1000];
    unsigned char bin_header[0x20];
    unsigned char root_cell[0x50];
    FILE *file = fopen(filename, "rb");
    if (!file) return 0;

    int valid = 0;
    if (fseek(file, 0, SEEK_END) != 0) goto done;
    long file_length = ftell(file);
    if (file_length < 0x2000 || fseek(file, 0, SEEK_SET) != 0) goto done;
    if (fread(header, 1, sizeof(header), file) != sizeof(header)) goto done;
    if (memcmp(header, "regf", 4) != 0) goto done;

    uint32_t root_offset = read_u32_le(header + 0x24);
    uint32_t hive_length = read_u32_le(header + 0x28);
    if (hive_length < 0x1000 || (uint64_t)hive_length + 0x1000 > (uint64_t)file_length ||
        root_offset < 0x20 || (uint64_t)root_offset + sizeof(root_cell) > hive_length)
        goto done;

    if (fseek(file, 0x1000, SEEK_SET) != 0 ||
        fread(bin_header, 1, sizeof(bin_header), file) != sizeof(bin_header))
        goto done;
    if (memcmp(bin_header, "hbin", 4) != 0 || read_u32_le(bin_header + 4) != 0 ||
        read_u32_le(bin_header + 8) < 0x1000 ||
        read_u32_le(bin_header + 8) > hive_length)
        goto done;

    if (fseek(file, 0x1000L + root_offset, SEEK_SET) != 0 ||
        fread(root_cell, 1, sizeof(root_cell), file) != sizeof(root_cell))
        goto done;
    int32_t cell_length = (int32_t)read_u32_le(root_cell);
    if (cell_length > -0x50 || memcmp(root_cell + 4, "nk", 2) != 0)
        goto done;
    valid = 1;

done:
    fclose(file);
    return valid;
}

struct hive *bls_hive_open(const char *filename) {
    if (!filename || !has_valid_root_cell(filename)) return NULL;
    struct hive *hive = openHive((char *)filename, HMODE_RO);
    if (hive && (hive->size < 0x1000 || hive->rootofs <= 0 ||
                 hive->rootofs >= hive->size - 4)) {
        closeHive(hive);
        return NULL;
    }
    return hive;
}

void bls_hive_close(struct hive *hive) {
    if (hive) closeHive(hive);
}

static int walk_key(struct hive *hive, int nkofs, const char *path,
                    bls_visit visit, void *context, int depth) {
    int count = 0, countri = 0, status;
    struct vex_data value;
    struct ex_data child;

    if (depth > 128 || visit(context, path, NULL, 0, NULL, 0)) return -2;

    while ((status = ex_next_v(hive, nkofs, &count, &value)) > 0) {
        if (value.name && (value.type == REG_BINARY || value.type == REG_QWORD ||
                           value.type == REG_DWORD)) {
            struct keyval *data = get_val2buf(hive, NULL, nkofs, value.name,
                                              value.type, TPF_VK_EXACT | TPF_ABS);
            if (!data) {
                free(value.name);
                return -2;
            }
            int failed = visit(context, path, value.name, value.type,
                               (const unsigned char *)&data->data, data->len);
            free(data);
            if (failed) {
                free(value.name);
                return -2;
            }
        }
        free(value.name);
    }
    if (status < 0) return -2;

    count = 0;
    while ((status = ex_next_n(hive, nkofs, &count, &countri, &child)) > 0) {
        if (!child.name) return -2;
        size_t parent_len = strlen(path), name_len = strlen(child.name);
        if (parent_len + name_len + 2 > ABSPATHLEN) {
            free(child.name);
            return -2;
        }
        char *next = malloc(parent_len + name_len + 2);
        if (!next) {
            free(child.name);
            return -2;
        }
        memcpy(next, path, parent_len);
        next[parent_len] = '\\';
        memcpy(next + parent_len + 1, child.name, name_len + 1);
        int result = walk_key(hive, child.nkoffs + 4, next, visit, context,
                              depth + 1);
        free(next);
        free(child.name);
        if (result) return result;
    }
    return status < 0 ? -2 : 0;
}

/* Returns 0 on success, -1 when the requested key is absent, -2 on error. */
int bls_hive_walk(struct hive *hive, const char *key, bls_visit visit,
                  void *context) {
    if (!hive || !key || !visit || strlen(key) >= ABSPATHLEN) return -2;
    int offset = trav_path(hive, 0, (char *)key, TPF_NK_EXACT);
    if (!offset) return -1;
    return walk_key(hive, offset + 4, key, visit, context, 0);
}

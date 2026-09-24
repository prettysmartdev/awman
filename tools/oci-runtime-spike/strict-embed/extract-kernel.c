#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

int main(int argc, char **argv) {
    if (argc != 3) {
        fprintf(stderr, "usage: extract-kernel FIRMWARE_LIBRARY OUTPUT_KERNEL\n");
        return 2;
    }
    void *library = dlopen(argv[1], RTLD_NOW | RTLD_LOCAL);
    if (!library) {
        fprintf(stderr, "%s\n", dlerror());
        return 1;
    }
    char *(*get_kernel)(uint64_t *, uint64_t *, size_t *) = dlsym(library, "krunfw_get_kernel");
    if (!get_kernel) {
        fprintf(stderr, "%s\n", dlerror());
        return 1;
    }
    uint64_t guest_address = 0, entry_address = 0;
    size_t size = 0;
    char *bytes = get_kernel(&guest_address, &entry_address, &size);
    if (!bytes || !size) return 1;
    FILE *output = fopen(argv[2], "wb");
    if (!output) return 1;
    if (fwrite(bytes, 1, size, output) != size || fclose(output)) return 1;
    printf("guest_address=%llu\nentry_address=%llu\nsize=%zu\n",
           (unsigned long long)guest_address, (unsigned long long)entry_address, size);
    dlclose(library);
    return 0;
}

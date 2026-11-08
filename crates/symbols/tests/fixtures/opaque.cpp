// A type that basic.cpp sees only as a forward declaration.
#include <stdint.h>

struct Opaque {
    uint32_t hidden_value;
};

Opaque g_opaque_object = {0x5A5A};

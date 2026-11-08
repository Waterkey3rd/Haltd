// Exercises the DWARF shapes the firmware uses. Values are asserted by
// crates/symbols/tests/fixture.rs, so keep the two in sync.
#include <stdint.h>

// `used` stops the compiler dropping these. The linker keeps them through the
// -u options in build.rs; file-local ones are referenced from entry() instead.
#define KEEP [[gnu::used]]

namespace fw {

enum class Mode : uint8_t { Idle = 0, Run = 3 };

struct Inner {
    int16_t a;
    uint8_t enabled : 1;
    int8_t level : 3;
    float gain;
};

struct Base {
    uint32_t base_id;
};

struct State : Base {
    Inner items[3];
    int matrix[2][3];
    Mode mode;
    char name[8];
    Inner* cursor;
    union {
        uint32_t word;
        float real;
    };
};

template <typename T, int N>
struct Buffer {
    T data[N];
    int size;
};

KEEP State g_state = {
    {0x11223344},
    {{-5, 1, -2, 1.5f}, {6, 0, 3, 2.5f}, {7, 1, -1, -0.25f}},
    {{1, 2, 3}, {4, 5, 6}},
    Mode::Run,
    "arm",
    &g_state.items[1],
    {0xABCD1234},
};

KEEP Buffer<float, 4> g_buffer = {{0.5f, 1.5f, 2.5f, 3.5f}, 4};

struct Widget {
    static int s_count;
};
KEEP int Widget::s_count = 77;

namespace {
KEEP int hidden = 5;
}

}  // namespace fw

KEEP volatile uint32_t g_counter = 0xCAFE;
KEEP fw::State* g_state_ptr = &fw::g_state;
KEEP fw::State& g_state_ref = fw::g_state;

// Unreferenced, so the linker discards it and DWARF records address 0.
int g_discarded = 9;

// Only declared here; the definition lives in opaque.cpp.
struct Opaque;
extern Opaque g_opaque_object;
KEEP Opaque* g_opaque = &g_opaque_object;

[[gnu::noinline]] int bump() {
    KEEP static int s_calls = 11;
    return ++s_calls;
}

extern "C" KEEP void prvTaskExitError() {
    while (true) {
    }
}

KEEP volatile int g_input = 3;

[[gnu::always_inline]] inline int add_twice(int v) {
    int doubled = v * 2;  // LINE:inline_body
    return doubled + g_input;
}

[[gnu::noinline]] int uses_inline(int base) {
    int total = base;  // LINE:before_inline
    total += add_twice(total);  // LINE:inline_call


    return total + g_input;  // LINE:after_gap
}

[[gnu::noinline]] int with_frame(int x) {
    return bump() + x;
}

extern "C" KEEP void entry() {
    with_frame(2);
    uses_inline(4);
    __asm__ volatile("" : : "r"(&fw::hidden));
    while (true) {
    }
}

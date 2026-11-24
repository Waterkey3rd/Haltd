#include <stdint.h>

struct TelemetryNoSync {
    int16_t speed;
    float current;
};

struct TelemetryWithSync {
    uint16_t sync;
    uint16_t seq;
    float iq;
};

typedef struct {
    uint32_t count;
    float voltage;
} TypedefFrame;

enum class Mode : uint8_t {
    Idle = 0,
    Run = 1,
};

[[gnu::used]] TelemetryNoSync g_no_sync{};
[[gnu::used]] TelemetryWithSync g_with_sync{};
[[gnu::used]] TypedefFrame g_typedef{};
[[gnu::used]] Mode g_mode = Mode::Idle;

extern "C" [[gnu::used]] void entry() {
    while (true) {
    }
}

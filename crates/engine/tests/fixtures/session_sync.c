#include <stdint.h>

volatile uint32_t g_cond = 0;

__attribute__((naked, noinline)) void inner_b(uint32_t arg) {
    (void)arg;
    __asm__ volatile(
        "nop\n"
        "bx lr\n"
    );
}

__attribute__((naked, noinline)) void outer_a(void) {
    __asm__ volatile(
        "push {r7, lr}\n"
        "movs r0, #41\n"
        "bl inner_b\n"
        "pop {r7, pc}\n"
    );
}

void entry(void) {
    outer_a();
    for (;;) {
    }
}

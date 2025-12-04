#include "stacktrace.h"
#include "../drivers/serial.h"
#include "cpu.h"

#define STACKTRACE_MAX_LOCAL 32

static inline uint64_t read_frame_pointer(void) {
    return cpu_read_rbp();
}

static int is_canonical_address(uint64_t address) {
    uint64_t upper = address >> 47;
    return upper == 0 || upper == 0x1FFFF;
}

static int basic_sanity_check(uint64_t current_rbp, uint64_t next_rbp) {
    if (next_rbp <= current_rbp) {
        return 0;
    }
    if ((next_rbp - current_rbp) > (1ULL << 20)) {
        return 0;
    }
    return 1;
}

int stacktrace_capture_from(uint64_t rbp,
                            struct stacktrace_entry *entries,
                            int max_entries) {
    if (!entries || max_entries <= 0) {
        return 0;
    }

    int count = 0;

    while (rbp != 0 && count < max_entries) {
        if ((rbp & 0x7) != 0 || !is_canonical_address(rbp)) {
            break;
        }

        uint64_t *frame = (uint64_t *)(uintptr_t)rbp;
        uint64_t next_rbp = frame[0];
        uint64_t return_address = frame[1];

        entries[count].frame_pointer = rbp;
        entries[count].return_address = return_address;
        count++;

        if (!is_canonical_address(next_rbp)) {
            break;
        }

        if (!basic_sanity_check(rbp, next_rbp)) {
            break;
        }

        rbp = next_rbp;
    }

    return count;
}

int stacktrace_capture(struct stacktrace_entry *entries, int max_entries) {
    uint64_t rbp = read_frame_pointer();
    return stacktrace_capture_from(rbp, entries, max_entries);
}

static void print_entry(int index, const struct stacktrace_entry *entry) {
    kprint("  #");
    kprint_decimal((uint64_t)index);
    kprint(" rbp=0x");
    kprint_hex(entry->frame_pointer);
    kprint(" rip=0x");
    kprint_hex(entry->return_address);
    kprintln("");
}

void stacktrace_dump_from(uint64_t rbp, int max_frames) {
    if (max_frames <= 0) {
        return;
    }

    if (max_frames > STACKTRACE_MAX_LOCAL) {
        max_frames = STACKTRACE_MAX_LOCAL;
    }

    struct stacktrace_entry entries[STACKTRACE_MAX_LOCAL];
    int captured = stacktrace_capture_from(rbp, entries, max_frames);

    if (captured <= 0) {
        kprintln("STACKTRACE: <empty>");
        return;
    }

    kprintln("STACKTRACE:");
    for (int i = 0; i < captured; i++) {
        print_entry(i, &entries[i]);
    }
}

void stacktrace_dump(int max_frames) {
    uint64_t rbp = read_frame_pointer();
    stacktrace_dump_from(rbp, max_frames);
}

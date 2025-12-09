# SlopOS Privilege Separation Architecture

## Overview

SlopOS implements full x86-64 privilege separation with Ring 0 (kernel mode) and Ring 3 (user mode) protection. This document describes the architecture, implementation, and verification of the privilege separation mechanisms.

## Architecture Components

### 1. Global Descriptor Table (GDT)

**Location:** `boot/gdt.c`, `boot/gdt_defs.h`

The GDT defines five primary segments plus a Task State Segment (TSS):

```c
// Kernel segments (Ring 0 / DPL=0)
#define GDT_CODE_SELECTOR             0x08     /* Kernel code segment selector (RPL0) */
#define GDT_DATA_SELECTOR             0x10     /* Kernel data segment selector (RPL0) */

// User segments (Ring 3 / DPL=3)
#define GDT_USER_DATA_SELECTOR        0x1B     /* User data segment selector (RPL3) */
#define GDT_USER_CODE_SELECTOR        0x23     /* User code segment selector (RPL3) */

// Task State Segment for privilege elevation
#define GDT_TSS_SELECTOR              0x28     /* Task State Segment selector */
```

**Key Features:**
- User segment selectors have RPL=3 (lowest two bits set to 11b), indicating Ring 3 privilege
- Kernel segment selectors have RPL=0, indicating Ring 0 privilege
- All segments are 64-bit long mode segments with appropriate DPL (Descriptor Privilege Level)

**Segment Descriptor Format:**

```c
#define GDT_CODE_DESCRIPTOR_64        0x00AF9A000000FFFFULL  /* DPL=0, executable, readable */
#define GDT_DATA_DESCRIPTOR_64        0x00AF92000000FFFFULL  /* DPL=0, writable */
#define GDT_USER_CODE_DESCRIPTOR_64   0x00AFFA000000FFFFULL  /* DPL=3, executable, readable */
#define GDT_USER_DATA_DESCRIPTOR_64   0x00AFF2000000FFFFULL  /* DPL=3, writable */
```

The DPL field is encoded in bits 45-46 of the descriptor:
- `0x9A` (kernel code): `10011010b` → DPL=00b (Ring 0)
- `0xFA` (user code):   `11111010b` → DPL=11b (Ring 3)

### 2. Task State Segment (TSS)

**Location:** `boot/gdt.c`

The TSS contains the kernel stack pointer (RSP0) used when transitioning from Ring 3 to Ring 0:

```c
struct tss64 {
    uint32_t reserved0;
    uint64_t rsp0;           /* Kernel stack for privilege elevation */
    uint64_t rsp1;
    uint64_t rsp2;
    uint64_t reserved1;
    uint64_t ist[7];         /* Interrupt Stack Table entries */
    uint64_t reserved2;
    uint16_t reserved3;
    uint16_t iomap_base;
} __attribute__((packed));
```

**RSP0 Management:**

When switching to a user task, the scheduler updates RSP0:

```c
void gdt_set_kernel_rsp0(uint64_t rsp0);
```

This ensures that when a user task triggers a syscall or exception, the CPU automatically switches to the kernel stack stored in RSP0.

### 3. Task Context Structure

**Location:** `sched/task.h`

Each task maintains a full CPU context including segment selectors:

```c
typedef struct task_context {
    /* General purpose registers */
    uint64_t rax, rbx, rcx, rdx;
    uint64_t rsi, rdi, rbp, rsp;
    uint64_t r8, r9, r10, r11;
    uint64_t r12, r13, r14, r15;

    /* Instruction pointer and flags */
    uint64_t rip;
    uint64_t rflags;

    /* Segment registers */
    uint64_t cs, ds, es, fs, gs, ss;

    /* Control registers */
    uint64_t cr3;  /* Page directory base register */
} __attribute__((packed)) task_context_t;
```

**Segment Register Initialization:**

User mode tasks (`TASK_FLAG_USER_MODE`):
```c
task->context.cs = GDT_USER_CODE_SELECTOR;  /* 0x23 - Ring 3 code */
task->context.ds = GDT_USER_DATA_SELECTOR;  /* 0x1B - Ring 3 data */
task->context.es = GDT_USER_DATA_SELECTOR;
task->context.ss = GDT_USER_DATA_SELECTOR;  /* Ring 3 stack */
```

Kernel mode tasks (`TASK_FLAG_KERNEL_MODE`):
```c
task->context.cs = GDT_CODE_SELECTOR;       /* 0x08 - Ring 0 code */
task->context.ds = GDT_DATA_SELECTOR;       /* 0x10 - Ring 0 data */
task->context.es = GDT_DATA_SELECTOR;
task->context.ss = GDT_DATA_SELECTOR;       /* Ring 0 stack */
```

### 4. Context Switching with Privilege Elevation

**Location:** `sched/context_switch.s`, `sched/scheduler.c`

#### User Mode Entry (Ring 0 → Ring 3)

The `context_switch_user()` function performs privilege demotion using the `iretq` instruction:

```asm
context_switch_user:
    /* Save kernel context if provided */
    test    %r8, %r8
    jz      csu_after_save
    /* ... save kernel registers ... */

csu_after_save:
    /* Load user data segments (DS, ES, FS, GS) */
    movq    0x98(%rbx), %rax        # user ds
    movw    %ax, %ds
    movq    0xA0(%rbx), %rax        # user es
    movw    %ax, %es
    /* ... */

    /* Build IRET frame on kernel stack:
     * [rsp+0]  = user RIP
     * [rsp+8]  = user CS (0x23 - Ring 3)
     * [rsp+16] = user RFLAGS
     * [rsp+24] = user RSP
     * [rsp+32] = user SS (0x1B - Ring 3)
     */
    pushq   %rax                    # user SS
    pushq   %rax                    # user RSP
    pushq   %rax                    # user RFLAGS
    pushq   %rax                    # user CS
    pushq   %rax                    # user RIP

    /* Switch to user page tables */
    movq    0xC0(%rbx), %rax
    movq    %rax, %cr3

    /* Load user general-purpose registers */
    /* ... */

    /* Transition to Ring 3 */
    iretq
```

**IRET Behavior:**
The `iretq` instruction atomically:
1. Pops RIP, CS, RFLAGS, RSP, SS from the stack
2. Checks CS.RPL (must be ≥ current CPL)
3. Sets CPL to CS.RPL (Ring 3)
4. Switches to user stack (RSP, SS)
5. Validates segment descriptors match the target privilege level
6. Resumes execution at user RIP with Ring 3 privileges

#### Scheduler Integration

**Location:** `sched/scheduler.c` (lines 301-313)

```c
if (new_task->flags & TASK_FLAG_USER_MODE) {
    /* Set RSP0 to task's kernel stack for future syscalls/interrupts */
    uint64_t rsp0 = new_task->kernel_stack_top ? 
                    new_task->kernel_stack_top : 
                    (uint64_t)&kernel_stack_top;
    gdt_set_kernel_rsp0(rsp0);
    
    /* Use IRET-based context switch to enter Ring 3 */
    context_switch_user(old_ctx_ptr, &new_task->context);
} else {
    /* Kernel task - use simple JMP-based switch (stays in Ring 0) */
    gdt_set_kernel_rsp0((uint64_t)&kernel_stack_top);
    context_switch(old_ctx_ptr, &new_task->context);
}
```

### 5. Syscall Gate (Ring 3 → Ring 0)

**Location:** `boot/idt.c`, `drivers/syscall.c`

#### IDT Configuration

The syscall gate is installed with DPL=3, allowing user mode to trigger it:

```c
/* boot/idt.c line 92 */
idt_set_gate_priv(SYSCALL_VECTOR, (uint64_t)isr128, 
                  GDT_CODE_SELECTOR, IDT_GATE_TRAP, 3);
                  /* vector 0x80 */      /* handler */
                  /* kernel CS */        /* trap gate */
                                         /* DPL=3 - user accessible */
```

**IDT Entry Format:**
```c
void idt_set_gate_priv(uint8_t vector, uint64_t handler, 
                      uint16_t selector, uint8_t type, uint8_t dpl) {
    idt[vector].offset_low = handler & 0xFFFF;
    idt[vector].selector = selector;
    idt[vector].ist = 0;
    idt[vector].type_attr = type | 0x80 | ((dpl & 0x3) << 5);
    /* Present=1, DPL in bits 5-6 */
    /* ... */
}
```

The DPL=3 in the type_attr field allows Ring 3 code to execute `int 0x80`.

#### Automatic Privilege Elevation

When a user task executes `int 0x80`:

1. **CPU automatically:**
   - Checks that IDT gate DPL ≥ CPL (3 ≥ 3 ✓)
   - Saves user SS and RSP
   - Loads kernel SS (from TSS)
   - Loads kernel RSP (RSP0 from TSS)
   - Pushes user SS, user RSP, RFLAGS, user CS, user RIP onto kernel stack
   - Sets CPL to target CS.DPL (Ring 0)
   - Jumps to interrupt handler (`isr128` → `syscall_handle`)

2. **Syscall handler** (`drivers/syscall.c`):
   - Receives interrupt frame with user context
   - Validates user pointers before dereferencing
   - Executes kernel operation
   - Returns to user mode via `iretq` (automatic privilege demotion)

**Interrupt Frame Structure:**

```c
struct interrupt_frame {
    /* Pushed by CPU automatically on privilege change */
    uint64_t rip;       /* User instruction pointer */
    uint64_t cs;        /* User code segment (0x23) */
    uint64_t rflags;    /* User flags */
    uint64_t rsp;       /* User stack pointer */
    uint64_t ss;        /* User stack segment (0x1B) */
    
    /* Registers saved by handler */
    uint64_t rax, rbx, rcx, rdx, rsi, rdi, rbp;
    uint64_t r8, r9, r10, r11, r12, r13, r14, r15;
    /* ... */
};
```

#### Syscall Context Preservation

**Location:** `drivers/syscall.c` (lines 27-60)

```c
static void save_user_context(struct interrupt_frame *frame, task_t *task) {
    task_context_t *ctx = &task->context;
    
    /* Save user register state */
    ctx->rax = frame->rax;
    ctx->rbx = frame->rbx;
    /* ... all registers ... */
    
    /* Save user segment selectors */
    ctx->rip = frame->rip;
    ctx->rsp = frame->rsp;
    ctx->cs = frame->cs;        /* Will be 0x23 (Ring 3) */
    ctx->ss = frame->ss;        /* Will be 0x1B (Ring 3) */
    ctx->ds = GDT_USER_DATA_SELECTOR;
    ctx->es = GDT_USER_DATA_SELECTOR;
    
    /* Mark context as captured from user mode */
    task->context_from_user = 1;
    task->user_started = 1;
}
```

This ensures that when the user task is rescheduled, it resumes in Ring 3 with correct privileges.

### 6. Memory Protection

**Location:** `mm/process_vm.c`, `mm/paging.h`

#### User/Kernel Page Table Separation

Each user task has its own address space:

```c
/* sched/task.c lines 206-222 */
if (flags & TASK_FLAG_USER_MODE) {
    /* Create isolated address space */
    process_id = create_process_vm();
    
    /* Allocate user stack with USER flag */
    stack_base = process_vm_alloc(process_id, TASK_STACK_SIZE,
                                  VM_FLAG_READ | 
                                  VM_FLAG_WRITE | 
                                  VM_FLAG_USER);
    
    /* Allocate separate kernel stack for syscalls */
    void *kstack = kmalloc(TASK_KERNEL_STACK_SIZE);
    task->kernel_stack_base = (uint64_t)kstack;
    task->kernel_stack_top = task->kernel_stack_base + TASK_KERNEL_STACK_SIZE;
}
```

**Page Table Entry Flags:**

```c
#define VM_FLAG_USER     0x04    /* Page accessible from Ring 3 */
```

The U/S bit (bit 2) in page table entries:
- 0 = Supervisor only (Ring 0-2)
- 1 = User accessible (Ring 0-3)

The higher-half kernel mappings remain supervisor-only; user page tables clone them without setting U/S. Only the dedicated `.user_text`, `.user_rodata`, and `.user_data` sections are marked U/S=1 so user tasks can execute their code without seeing kernel internals. Any additional sharing must use an explicit helper, keeping the default template locked down.

### User-mapped kernel sections
- `.user_text` holds user-executable code; `task_create` rejects user tasks whose entry RIP is outside this window.
- `.user_rodata` and `.user_data` carry read-only and writable globals used by user tasks.
- Kernel heap/text/data/bss remain supervisor-only; `user_copy_*` guards log and fail if kernel heap ever becomes user-accessible.

#### Safe User Memory Access

**Location:** `mm/user_copy.c`, `mm/user_copy.h`

The kernel validates all user pointers before dereferencing:

```c
/* Validate ring3 buffers before touching them */
int user_copy_from_user(void *kernel_dst, const void *user_src, size_t n);
int user_copy_to_user(void *user_dst, const void *kernel_src, size_t n);
```

Validation now requires mappings to be present *and* marked user-accessible; a once-per-boot guard rejects operation if any kernel virtual address is ever observed as user-accessible.

This prevents:
- User code accessing kernel memory
- Kernel accidentally dereferencing invalid user pointers
- Time-of-check-time-of-use (TOCTOU) vulnerabilities

### 7. Privilege Verification

**Location:** `sched/test_tasks.c` (lines 207-268)

The privilege separation invariant test verifies:

```c
int run_privilege_separation_invariant_test(void) {
    /* 1. Create user mode task */
    uint32_t user_task_id = task_create("UserStub", user_stub_task, NULL,
                                        TASK_PRIORITY_NORMAL,
                                        TASK_FLAG_USER_MODE);
    
    task_t *task_info;
    task_get_info(user_task_id, &task_info);
    
    /* 2. Verify task has isolated process VM */
    if (task_info->process_id == INVALID_PROCESS_ID) {
        return -1;  /* FAIL */
    }
    
    /* 3. Verify task has kernel RSP0 stack */
    if (task_info->kernel_stack_top == 0) {
        return -1;  /* FAIL */
    }
    
    /* 4. Verify segment selectors are Ring 3 */
    if (task_info->context.cs != GDT_USER_CODE_SELECTOR ||
        task_info->context.ss != GDT_USER_DATA_SELECTOR) {
        klog_printf(KLOG_INFO, "PRIVSEP_TEST: selectors incorrect (cs=0x%lx ss=0x%lx)\n",
                    task_info->context.cs, task_info->context.ss);
        return -1;  /* FAIL */
    }
    
    /* 5. Verify syscall gate is DPL=3 (user accessible) */
    struct idt_entry gate;
    idt_get_gate(SYSCALL_VECTOR, &gate);
    uint8_t dpl = (gate.type_attr >> 5) & 0x3;
    if (dpl != 3) {
        klog_printf(KLOG_INFO, "PRIVSEP_TEST: syscall gate DPL=%u expected 3\n", dpl);
        return -1;  /* FAIL */
    }
    
    return 0;  /* PASS */
}
```

### User fault reporting
- Exceptions raised from Ring 3 set `exit_reason=TASK_EXIT_REASON_USER_FAULT` and a `fault_reason` enum (`TASK_FAULT_USER_PAGE`, `TASK_FAULT_USER_GP`, `TASK_FAULT_USER_UD`, `TASK_FAULT_USER_DEVICE_NA`).
- The fault is logged, `wl_award_loss()` is applied, and `task_get_exit_record()` can retrieve the structured status after termination.

## Privilege Levels Summary

| Component | Ring 0 (Kernel) | Ring 3 (User) |
|-----------|-----------------|---------------|
| Code Segment | 0x08 (DPL=0, RPL=0) | 0x23 (DPL=3, RPL=3) |
| Data Segment | 0x10 (DPL=0, RPL=0) | 0x1B (DPL=3, RPL=3) |
| Stack Segment | 0x10 (DPL=0, RPL=0) | 0x1B (DPL=3, RPL=3) |
| Page Access | U/S=0 (supervisor) | U/S=1 (user) |
| Syscall Entry | DPL=3 (accessible) | int 0x80 allowed |
| CPU Privilege (CPL) | 0 | 3 |

## Execution Flow Example

### User Task Lifecycle

1. **Task Creation** (`task_create` with `TASK_FLAG_USER_MODE`):
   - Allocate isolated page directory
   - Allocate user stack (with U/S=1 pages)
   - Allocate separate kernel stack for RSP0
   - Initialize context with Ring 3 segment selectors

2. **First Schedule**:
   - Scheduler sets TSS.RSP0 = task's kernel stack
   - Calls `context_switch_user()`
   - `iretq` transitions to Ring 3
   - Task begins execution in user mode

3. **Syscall** (user code executes `int 0x80`):
   - CPU checks IDT[0x80].DPL ≥ CPL (3 ≥ 3 ✓)
   - CPU saves user SS:RSP
   - CPU loads TSS.RSP0 (kernel stack)
   - CPU pushes interrupt frame
   - CPU sets CPL=0
   - Handler executes in Ring 0
   - Handler returns via `iretq` → back to Ring 3

4. **Context Switch to Another Task**:
   - Timer interrupt or voluntary yield
   - Scheduler saves user context (via interrupt frame)
   - Marks `context_from_user = 1`
   - Next schedule restores context via `context_switch_user()`

## Current User Mode Tasks

**Location:** `boot/early_init.c`, `video/roulette_user.c`, `shell/shell.c`

1. **Roulette Task** (created at boot):
   ```c
   /* boot/early_init.c line 553 */
   task_create("roulette", roulette_user_main, NULL, 5, TASK_FLAG_USER_MODE);
   ```

2. **Shell Task** (spawned via syscall):
   ```c
   /* shell/shell.c */
   task_create("shell", shell_user_main, NULL, 5, TASK_FLAG_USER_MODE);
   ```

Both tasks:
- Run in Ring 3
- Have isolated address spaces
- Use syscalls for kernel services
- Cannot directly access kernel memory

## Verification

To verify privilege separation is working:

1. **Build and run the kernel:**
   ```bash
   make build
   make iso
   make boot
   ```

2. **Check boot logs for:**
   - `"GDT: Initialized with TSS loaded"` - GDT setup complete
   - `"Roulette task created and scheduled successfully!"` - User task running
   - No general protection faults during user task execution

3. **Run privilege test manually** (if integrated into test harness):
   - Test verifies segment selectors, RSP0, and syscall gate DPL

## Security Properties

The privilege separation enforces:

1. **Memory Isolation**: User tasks cannot read/write kernel memory
2. **Execution Control**: User tasks cannot execute privileged instructions
3. **Resource Protection**: System resources only accessible via syscalls
4. **Stack Isolation**: Separate kernel stacks prevent stack overflow attacks
5. **Validated Transitions**: All Ring 3→0 transitions go through IDT gates

## References

- Intel 64 and IA-32 Architectures Software Developer's Manual, Volume 3A
  - Chapter 3: Protected-Mode Memory Management
  - Chapter 5: Protection
  - Chapter 6: Interrupt and Exception Handling

- AMD64 Architecture Programmer's Manual, Volume 2
  - Chapter 4: Segmentation
  - Chapter 8: Exceptions and Interrupts

## Maintainers

- Leon Liechti (@Lon60) - Context switching, TSS, scheduler integration
- Fabrice Schaub (@Fabbboy) - GDT setup, initial design
- Luis (@ienjir) - Syscall infrastructure, testing

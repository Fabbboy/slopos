//! CPU feature detection via CPUID instruction.
//!
//! This module provides constants for CPUID feature flags used to detect
//! hardware capabilities like APIC, x2APIC, and other CPU features.

// =============================================================================
// CPUID Leaf Numbers
// =============================================================================

/// Basic CPU information and feature flags.
pub const CPUID_LEAF_FEATURES: u32 = 0x01;

/// Extended topology enumeration.
pub const CPUID_LEAF_TOPOLOGY: u32 = 0x0B;

/// Processor extended state enumeration.
pub const CPUID_LEAF_XSAVE: u32 = 0x0D;

/// Extended function information.
pub const CPUID_LEAF_EXT_INFO: u32 = 0x8000_0001;

// =============================================================================
// CPUID Leaf 1 - EDX Feature Flags
// =============================================================================

/// Floating Point Unit on-chip.
pub const CPUID_FEAT_EDX_FPU: u32 = 1 << 0;

/// Virtual Mode Extensions.
pub const CPUID_FEAT_EDX_VME: u32 = 1 << 1;

/// Debugging Extensions.
pub const CPUID_FEAT_EDX_DE: u32 = 1 << 2;

/// Page Size Extension (4MB pages).
pub const CPUID_FEAT_EDX_PSE: u32 = 1 << 3;

/// Time Stamp Counter.
pub const CPUID_FEAT_EDX_TSC: u32 = 1 << 4;

/// Model Specific Registers.
pub const CPUID_FEAT_EDX_MSR: u32 = 1 << 5;

/// Physical Address Extension.
pub const CPUID_FEAT_EDX_PAE: u32 = 1 << 6;

/// Machine Check Exception.
pub const CPUID_FEAT_EDX_MCE: u32 = 1 << 7;

/// CMPXCHG8B instruction.
pub const CPUID_FEAT_EDX_CX8: u32 = 1 << 8;

/// APIC present (on-chip Advanced Programmable Interrupt Controller).
pub const CPUID_FEAT_EDX_APIC: u32 = 1 << 9;

/// SYSENTER/SYSEXIT instructions.
pub const CPUID_FEAT_EDX_SEP: u32 = 1 << 11;

/// Memory Type Range Registers.
pub const CPUID_FEAT_EDX_MTRR: u32 = 1 << 12;

/// Page Global Enable.
pub const CPUID_FEAT_EDX_PGE: u32 = 1 << 13;

/// Machine Check Architecture.
pub const CPUID_FEAT_EDX_MCA: u32 = 1 << 14;

/// Conditional Move instructions.
pub const CPUID_FEAT_EDX_CMOV: u32 = 1 << 15;

/// Page Attribute Table.
pub const CPUID_FEAT_EDX_PAT: u32 = 1 << 16;

/// 36-bit Page Size Extension.
pub const CPUID_FEAT_EDX_PSE36: u32 = 1 << 17;

/// CLFLUSH instruction.
pub const CPUID_FEAT_EDX_CLFSH: u32 = 1 << 19;

/// MMX technology.
pub const CPUID_FEAT_EDX_MMX: u32 = 1 << 23;

/// FXSAVE/FXRSTOR instructions.
pub const CPUID_FEAT_EDX_FXSR: u32 = 1 << 24;

/// SSE extensions.
pub const CPUID_FEAT_EDX_SSE: u32 = 1 << 25;

/// SSE2 extensions.
pub const CPUID_FEAT_EDX_SSE2: u32 = 1 << 26;

// =============================================================================
// CPUID Leaf 1 - ECX Feature Flags
// =============================================================================

/// SSE3 extensions.
pub const CPUID_FEAT_ECX_SSE3: u32 = 1 << 0;

/// PCLMULQDQ instruction.
pub const CPUID_FEAT_ECX_PCLMULQDQ: u32 = 1 << 1;

/// MONITOR/MWAIT instructions.
pub const CPUID_FEAT_ECX_MONITOR: u32 = 1 << 3;

/// Supplemental SSE3 (SSSE3).
pub const CPUID_FEAT_ECX_SSSE3: u32 = 1 << 9;

/// SSE4.1 extensions.
pub const CPUID_FEAT_ECX_SSE41: u32 = 1 << 19;

/// SSE4.2 extensions.
pub const CPUID_FEAT_ECX_SSE42: u32 = 1 << 20;

/// x2APIC support.
pub const CPUID_FEAT_ECX_X2APIC: u32 = 1 << 21;

/// POPCNT instruction.
pub const CPUID_FEAT_ECX_POPCNT: u32 = 1 << 23;

/// AES-NI instruction set.
pub const CPUID_FEAT_ECX_AESNI: u32 = 1 << 25;

/// XSAVE/XRSTOR instructions.
pub const CPUID_FEAT_ECX_XSAVE: u32 = 1 << 26;

/// OS has enabled XSAVE.
pub const CPUID_FEAT_ECX_OSXSAVE: u32 = 1 << 27;

/// AVX extensions.
pub const CPUID_FEAT_ECX_AVX: u32 = 1 << 28;

/// Hypervisor present (running in VM).
pub const CPUID_FEAT_ECX_HYPERVISOR: u32 = 1 << 31;

// =============================================================================
// CPUID Extended Leaf 0x80000001 - EDX Flags
// =============================================================================

/// SYSCALL/SYSRET instructions (64-bit mode).
pub const CPUID_EXT_FEAT_EDX_SYSCALL: u32 = 1 << 11;

/// Execute Disable bit.
pub const CPUID_EXT_FEAT_EDX_NX: u32 = 1 << 20;

/// 1GB pages.
pub const CPUID_EXT_FEAT_EDX_PAGE1GB: u32 = 1 << 26;

/// RDTSCP instruction.
pub const CPUID_EXT_FEAT_EDX_RDTSCP: u32 = 1 << 27;

/// Long mode (64-bit).
pub const CPUID_EXT_FEAT_EDX_LM: u32 = 1 << 29;

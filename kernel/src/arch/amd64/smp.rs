//! Starting the application processors: INIT, then start-up IPIs to a
//! trampoline below 1 MiB that climbs from real mode to long mode and jumps
//! into the kernel.
//!
//! The trampoline is assembled into the kernel image and copied to two
//! free low pages: the code, and a copy of the kernel's top-level page
//! table, because in 32-bit mode CR3 can only point below 4 GiB. That copy
//! keeps the identity map (so the trampoline stays mapped when paging comes
//! on) and the kernel half. The CPUs start one at a time, each with its own
//! parameters in the trampoline's data block.

use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicUsize, Ordering};

use acpi::MadtEntry;
use bootinfo::phys_to_virt;

use super::{apic, trap};
use crate::port::acpi::System;
use crate::port::clock;
use crate::port::cpu::{self, Census, Cpu};
use crate::port::mm::{self, PAGE};
use crate::println;

const PAGE_BYTES: usize = 4096;

// Real mode at `page:0` → protected mode → long mode on the copied page
// table → the kernel's own CR0/CR4/CR3, stack, and `ap_entry(index)`.
// Fields the boot CPU fills are named; three hold offsets from the start
// that it turns into physical addresses.
global_asm!(
    r#"
    .section .text.trampoline, "ax"
    .balign 16
    .global planr_ap_start
planr_ap_start:
    .code16
    cli
    cld
    xorl %ebx, %ebx
    movw %cs, %bx
    movw %bx, %ds
    shll $4, %ebx                       // ebx = physical base
    lgdtl planr_ap_gdtr - planr_ap_start
    movl %cr0, %eax
    orl $1, %eax                        // PE
    movl %eax, %cr0
    ljmpl *(planr_ap_far32 - planr_ap_start)

    .code32
.Lprotected:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movl %cr4, %eax
    orl $0x20, %eax                     // PAE
    movl %eax, %cr4
    movl (planr_ap_params - planr_ap_start)(%ebx), %eax
    movl %eax, %cr3                     // the low copy of the page table
    movl $0xc0000080, %ecx              // IA32_EFER
    rdmsr
    orl $0x900, %eax                    // LME, NXE
    wrmsr
    movl %cr0, %eax
    orl $0x80010000, %eax               // PG, WP
    movl %eax, %cr0
    ljmpl *(planr_ap_far64 - planr_ap_start)(%ebx)

    .code64
.Llong:
    leaq (planr_ap_params - planr_ap_start)(%rbx), %rsi
    movq 8(%rsi), %rax
    movq %rax, %cr0
    movq 16(%rsi), %rax
    movq %rax, %cr4
    movq 24(%rsi), %rax
    movq %rax, %cr3                     // the kernel's page table
    movq 32(%rsi), %rsp
    movq 40(%rsi), %rdi
    movq 48(%rsi), %rax
    xorl %ebp, %ebp
    pushq $0                            // no return address
    jmpq *%rax

    .balign 8
.Lgdt:
    .quad 0
    .quad 0x00cf9a000000ffff            // 0x08: 32-bit code
    .quad 0x00cf92000000ffff            // 0x10: data
    .quad 0x00af9a000000ffff            // 0x18: 64-bit code
    .global planr_ap_gdtr
planr_ap_gdtr:
    .word 31
    .long .Lgdt - planr_ap_start
    .global planr_ap_far32
planr_ap_far32:
    .long .Lprotected - planr_ap_start
    .word 0x08
    .global planr_ap_far64
planr_ap_far64:
    .long .Llong - planr_ap_start
    .word 0x18
    .balign 8
    .global planr_ap_params
planr_ap_params:
    .fill 7, 8, 0
    .global planr_ap_end
planr_ap_end:
    .code64
    "#,
    options(att_syntax),
);

unsafe extern "C" {
    static planr_ap_start: u8;
    static planr_ap_gdtr: u8;
    static planr_ap_far32: u8;
    static planr_ap_far64: u8;
    static planr_ap_params: u8;
    static planr_ap_end: u8;
}

/// The trampoline's data block, at `planr_ap_params`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Params {
    /// The low page-table copy (below 4 GiB).
    low_cr3: u64,
    cr0: u64,
    cr4: u64,
    cr3: u64,
    stack: u64,
    index: u64,
    entry: u64,
}

/// Offset of a trampoline symbol from its start.
fn offset(sym: *const u8) -> usize {
    sym as usize - (&raw const planr_ap_start) as usize
}

/// The installed trampoline.
#[derive(Debug, Clone, Copy)]
struct Trampoline {
    /// Physical address of the code page; the page table copy follows.
    phys: u64,
}

impl Trampoline {
    /// Copies the trampoline and the top-level page table into low memory
    /// and fixes up its absolute addresses.
    fn install() -> Result<Trampoline, &'static str> {
        let len = offset(&raw const planr_ap_end);
        let phys = mm::low_pages(2).ok_or("no free memory below 1 MiB for the AP trampoline")?;
        let code = phys_to_virt(phys) as *mut u8;
        let base = u32::try_from(phys).map_err(|_| "trampoline above 4 GiB")?;
        // SAFETY: the two low pages are free RAM in the direct map and ours;
        // the template is `len` bytes of the kernel image, and `len` fits in
        // a page. The fixups are u32 fields inside the copy.
        unsafe {
            core::ptr::copy_nonoverlapping(&raw const planr_ap_start, code, len);
            for field in [
                offset(&raw const planr_ap_gdtr) + 2,
                offset(&raw const planr_ap_far32),
                offset(&raw const planr_ap_far64),
            ] {
                #[allow(clippy::cast_ptr_alignment)] // accessed unaligned
                let p = code.add(field).cast::<u32>();
                p.write_unaligned(p.read_unaligned() + base);
            }
            let pml4 = phys_to_virt(read_cr3() & !0xfff) as *const u8;
            core::ptr::copy_nonoverlapping(pml4, code.add(PAGE_BYTES), PAGE_BYTES);
        }
        Ok(Trampoline { phys })
    }

    /// Writes the parameters for the next CPU to start.
    fn set(self, stack: u64, index: usize) {
        let p = Params {
            low_cr3: self.phys + PAGE,
            cr0: read_cr0(),
            cr4: super::cpu::read_cr4(),
            cr3: read_cr3(),
            stack,
            index: index as u64,
            entry: ap_entry as *const () as u64,
        };
        let at = phys_to_virt(self.phys) + offset(&raw const planr_ap_params) as u64;
        // SAFETY: the data block is inside the copied page, 8-byte aligned.
        unsafe { (at as *mut Params).write_volatile(p) };
    }

    #[allow(clippy::cast_possible_truncation)] // below 1 MiB, so the page number fits
    fn page(self) -> u8 {
        (self.phys / PAGE) as u8
    }
}

fn read_cr0() -> u64 {
    let v: u64;
    // SAFETY: reads a control register.
    unsafe { asm!("mov {}, cr0", out(reg) v, options(nomem, nostack)) };
    v
}

fn read_cr3() -> u64 {
    let v: u64;
    // SAFETY: reads a control register.
    unsafe { asm!("mov {}, cr3", out(reg) v, options(nomem, nostack)) };
    v
}

/// The index of the CPU that has most recently reached [`ap_entry`]; tells
/// the boot CPU a start-up IPI took.
static ARRIVED: AtomicUsize = AtomicUsize::new(0);

/// Starts every enabled processor in the MADT other than this one.
pub(crate) fn start_cpus(acpi: Option<&System>) -> Result<Census, &'static str> {
    let madt = acpi.and_then(|s| s.tables.madt()).ok_or("no MADT")?;
    let me = apic::id();
    let ids = madt.entries().filter_map(|e| match e {
        MadtEntry::LocalApic {
            apic_id, enabled: true, ..
        } => Some(u32::from(apic_id)),
        MadtEntry::X2Apic {
            x2apic_id,
            enabled: true,
            ..
        } => Some(x2apic_id),
        _ => None,
    });
    let mut found = 1;
    let mut trampoline = None;
    for id in ids.filter(|&id| id != me) {
        found += 1;
        if !apic::addressable(id) {
            println!("smp: APIC ID {id} needs x2APIC; not started");
            continue;
        }
        let t = match trampoline {
            Some(t) => t,
            None => *trampoline.insert(Trampoline::install()?),
        };
        if let Err(e) = start(t, id) {
            println!("smp: CPU with APIC ID {id}: {e}");
        }
    }
    Ok(Census { found })
}

/// Starts the CPU with APIC ID `id` and waits for it to come online.
fn start(t: Trampoline, id: u32) -> Result<(), &'static str> {
    let cpu = cpu::claim(u64::from(id)).ok_or("too many CPUs")?;
    let index = cpu.index();
    let stack = mm::alloc_stack(mm::STACK_PAGES).ok_or("no memory for a stack")?;
    let ist_pages = (trap::IST_BYTES as u64).div_ceil(PAGE);
    let mut ist = [0; 3];
    for top in &mut ist {
        *top = mm::alloc_stack(ist_pages).ok_or("no memory for a stack")?;
    }
    trap::prepare(index, ist);
    t.set(stack, index);

    // The MP start-up sequence: INIT, 10 ms, start-up IPI, and a second
    // one if the first was not taken. Modern CPUs need far less than the
    // traditional delays, but emulators and old boards do not mind them.
    apic::send_init(id);
    clock::spin_until(|| false, 10_000_000);
    let arrived = || ARRIVED.load(Ordering::Acquire) == index;
    for _ in 0..2 {
        apic::send_startup(id, t.page());
        if clock::spin_until(arrived, 10_000_000) {
            break;
        }
    }
    if !cpu::wait_online(cpu) {
        // Park it again, so a late start cannot use the next CPU's stack.
        apic::send_init(id);
        return Err("did not come online");
    }
    Ok(())
}

/// Where the trampoline lands, on the CPU's own stack with the kernel's
/// page tables.
extern "C" fn ap_entry(index: usize) -> ! {
    ARRIVED.store(index, Ordering::Release);
    super::mmu::init_cpu();
    super::protect::init_cpu();
    trap::load(index);
    let Some(cpu) = cpu::get(index) else {
        super::halt();
    };
    super::set_cpu_local(cpu);
    apic::init_local();
    apic::init_local_timer();
    cpu::ap_main(cpu)
}

/// The calling CPU's hardware ID.
pub(crate) fn hwid() -> u64 {
    u64::from(apic::id())
}

/// Sends `vector` to `cpu`.
pub(crate) fn send_ipi(cpu: &Cpu, vector: usize) {
    if let (Ok(dest), Ok(v)) = (u32::try_from(cpu.hwid()), u8::try_from(vector)) {
        apic::ipi(dest, v);
    }
}

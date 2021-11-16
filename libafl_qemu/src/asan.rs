use libafl::{executors::ExitKind, inputs::Input, observers::ObserversTuple, state::HasMetadata};
use num_enum::{IntoPrimitive, TryFromPrimitive};
use std::{env, fs, ptr};

use crate::{
    emu,
    emu::SyscallHookResult,
    executor::QemuExecutor,
    helper::{QemuHelper, QemuHelperTuple, QemuInstrumentationFilter},
    Regs,
};

// TODO at some point, merge parts with libafl_frida

pub const QASAN_FAKESYS_NR: i32 = 0xa2a4;

#[derive(IntoPrimitive, TryFromPrimitive, Debug, Clone, Copy)]
#[repr(u64)]
pub enum QasanAction {
    CheckLoad,
    CheckStore,
    Poison,
    UserPoison,
    UnPoison,
    IsPoison,
    Alloc,
    Dealloc,
    Enable,
    Disable,
    SwapState,
}

#[derive(IntoPrimitive, TryFromPrimitive, Debug, Clone, Copy)]
#[repr(u8)]
pub enum PoisonKind {
    Valid = 0,
    Partial1 = 1,
    Partial2 = 2,
    Partial3 = 3,
    Partial4 = 4,
    Partial5 = 5,
    Partial6 = 6,
    Partial7 = 7,
    ArrayCookie = 0xac,
    StackRz = 0xf0,
    StackLeftRz = 0xf1,
    StackMidRz = 0xf2,
    StackRightRz = 0xf3,
    StacKFreed = 0xf5,
    StackOOScope = 0xf8,
    GlobalRz = 0xf9,
    HeapRz = 0xe9,
    User = 0xf7,
    HeapLeftRz = 0xfa,
    HeapRightRz = 0xfb,
    HeapFreed = 0xfd,
}

#[repr(C)]
struct CallContext {
    pub addresses: *const u64,
    pub tid: i32,
    pub size: u32,
}

#[repr(C)]
struct ChunkInfo {
    pub start: u64,
    pub end: u64,
    pub alloc_ctx: *const CallContext,
    pub free_ctx: *const CallContext, // NULL if chunk is allocated
}

extern "C" {
    fn asan_giovese_init();
    fn asan_giovese_load1(ptr: *const u8) -> i32;
    fn asan_giovese_load2(ptr: *const u8) -> i32;
    fn asan_giovese_load4(ptr: *const u8) -> i32;
    fn asan_giovese_load8(ptr: *const u8) -> i32;
    fn asan_giovese_store1(ptr: *const u8) -> i32;
    fn asan_giovese_store2(ptr: *const u8) -> i32;
    fn asan_giovese_store4(ptr: *const u8) -> i32;
    fn asan_giovese_store8(ptr: *const u8) -> i32;
    fn asan_giovese_loadN(ptr: *const u8, n: usize) -> i32;
    fn asan_giovese_storeN(ptr: *const u8, n: usize) -> i32;
    fn asan_giovese_poison_region(ptr: *const u8, n: usize, poison: u8) -> i32;
    fn asan_giovese_unpoison_region(ptr: *const u8, n: usize) -> i32;
    fn asan_giovese_alloc_search(query: u64) -> *mut ChunkInfo;
    fn asan_giovese_alloc_remove(start: u64, end: u64);
    fn asan_giovese_alloc_insert(start: u64, end: u64, alloc_ctx: *const CallContext);
    fn asan_giovese_report_and_crash(
        access_type: i32,
        addr: u64,
        n: usize,
        pc: u64,
        bp: u64,
        sp: u64,
    );
    fn asan_giovese_badfree(addr: u64, pc: u64);
}

#[no_mangle]
extern "C" fn asan_giovese_printaddr(_addr: u64) -> *const u8 {
    // Just addresses ATM
    ptr::null()
}

#[no_mangle]
unsafe extern "C" fn asan_giovese_populate_context(ctx: *mut CallContext, _pc: u64) {
    let ctx = ctx.as_mut().unwrap();
    ctx.tid = libc::gettid() as i32;
    ctx.size = 0;
}

static mut ASAN_INITED: bool = false;

pub fn init_with_asan(args: &mut Vec<String>, env: &mut [(String, String)]) -> i32 {
    assert!(!args.is_empty());
    let current = env::current_exe().unwrap();
    let asan_lib = fs::canonicalize(&current)
        .unwrap()
        .parent()
        .unwrap()
        .join("libqasan.so");
    let asan_lib = asan_lib
        .to_str()
        .expect("The path to the asan lib is invalid")
        .to_string();
    let add_asan =
        |e: &str| "LD_PRELOAD=".to_string() + &asan_lib + " " + &e["LD_PRELOAD=".len()..];

    let mut added = false;
    for (k, v) in env.iter_mut() {
        if k == "QEMU_SET_ENV" {
            let mut new_v = vec![];
            for e in v.split(",") {
                if e.starts_with("LD_PRELOAD=") {
                    added = true;
                    new_v.push(add_asan(e));
                } else {
                    new_v.push(e.to_string());
                }
            }
            *v = new_v.join(",");
        }
    }
    for i in 0..args.len() {
        if args[i] == "-E" && i + 1 < args.len() {
            if args[i + 1].starts_with("LD_PRELOAD=") {
                added = true;
                args[i + 1] = add_asan(&args[i + 1])
            }
        }
    }

    if !added {
        args.insert(1, "LD_PRELOAD=".to_string() + &asan_lib);
        args.insert(1, "-E".into());
    }

    unsafe {
        asan_giovese_init();
        ASAN_INITED = true;
    }
    emu::init(args, env)
}

// TODO intrumentation filter
pub struct QemuAsanHelper {
    enabled: bool,
    filter: QemuInstrumentationFilter,
}

impl QemuAsanHelper {
    #[must_use]
    pub fn new() -> Self {
        assert!(unsafe { ASAN_INITED == true }, "The ASan runtime is not initialized, use init_with_asan(...) instead of just init(...)");
        Self {
            enabled: true,
            filter: QemuInstrumentationFilter::None,
        }
    }

    #[must_use]
    pub fn with_instrumentation_filter(filter: QemuInstrumentationFilter) -> Self {
        Self {
            enabled: true,
            filter,
        }
    }

    #[must_use]
    pub fn must_instrument(&self, addr: u64) -> bool {
        self.filter.allowed(addr)
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    #[allow(clippy::unused_self)]
    pub fn alloc(&mut self, start: u64, end: u64) {
        unsafe {
            let ctx: *const CallContext =
                libc::calloc(core::mem::size_of::<CallContext>(), 1) as *const _;
            asan_giovese_alloc_insert(start, end, ctx);
        }
    }

    #[allow(clippy::unused_self)]
    pub fn dealloc(&mut self, addr: u64) {
        unsafe {
            let ckinfo = asan_giovese_alloc_search(addr);
            if let Some(ck) = ckinfo.as_mut() {
                if ck.start != addr {
                    // Free not the start of the chunk
                    asan_giovese_badfree(addr, emu::read_reg(Regs::Pc).unwrap_or(u64::MAX));
                }
                let ctx: *const CallContext =
                    libc::calloc(core::mem::size_of::<CallContext>(), 1) as *const _;
                ck.free_ctx = ctx;
            } else {
                // Free of wild ptr
                asan_giovese_badfree(addr, emu::read_reg(Regs::Pc).unwrap_or(u64::MAX));
            }
        }
    }

    #[allow(clippy::unused_self)]
    pub fn is_poisoned(&self, addr: u64, size: usize) -> bool {
        unsafe { asan_giovese_loadN(emu::g2h(addr), size) != 0 }
    }

    pub fn read_1(&mut self, addr: u64) {
        unsafe {
            if self.enabled() && asan_giovese_load1(emu::g2h(addr)) != 0 {
                asan_giovese_report_and_crash(
                    0,
                    addr,
                    1,
                    emu::read_reg(Regs::Pc).unwrap_or(u64::MAX),
                    0,
                    emu::read_reg(Regs::Sp).unwrap_or(u64::MAX),
                );
            }
        }
    }

    pub fn read_2(&mut self, addr: u64) {
        unsafe {
            if self.enabled() && asan_giovese_load2(emu::g2h(addr)) != 0 {
                asan_giovese_report_and_crash(
                    0,
                    addr,
                    2,
                    emu::read_reg(Regs::Pc).unwrap_or(u64::MAX),
                    0,
                    emu::read_reg(Regs::Sp).unwrap_or(u64::MAX),
                );
            }
        }
    }

    pub fn read_4(&mut self, addr: u64) {
        unsafe {
            if self.enabled() && asan_giovese_load4(emu::g2h(addr)) != 0 {
                asan_giovese_report_and_crash(
                    0,
                    addr,
                    4,
                    emu::read_reg(Regs::Pc).unwrap_or(u64::MAX),
                    0,
                    emu::read_reg(Regs::Sp).unwrap_or(u64::MAX),
                );
            }
        }
    }

    pub fn read_8(&mut self, addr: u64) {
        unsafe {
            if self.enabled() && asan_giovese_load8(emu::g2h(addr)) != 0 {
                asan_giovese_report_and_crash(
                    0,
                    addr,
                    8,
                    emu::read_reg(Regs::Pc).unwrap_or(u64::MAX),
                    0,
                    emu::read_reg(Regs::Sp).unwrap_or(u64::MAX),
                );
            }
        }
    }

    pub fn read_n(&mut self, addr: u64, size: usize) {
        unsafe {
            if self.enabled() && asan_giovese_loadN(emu::g2h(addr), size) != 0 {
                asan_giovese_report_and_crash(
                    0,
                    addr,
                    size,
                    emu::read_reg(Regs::Pc).unwrap_or(u64::MAX),
                    0,
                    emu::read_reg(Regs::Sp).unwrap_or(u64::MAX),
                );
            }
        }
    }

    pub fn write_1(&mut self, addr: u64) {
        unsafe {
            if self.enabled() && asan_giovese_store1(emu::g2h(addr)) != 0 {
                asan_giovese_report_and_crash(
                    1,
                    addr,
                    1,
                    emu::read_reg(Regs::Pc).unwrap_or(u64::MAX),
                    0,
                    emu::read_reg(Regs::Sp).unwrap_or(u64::MAX),
                );
            }
        }
    }

    pub fn write_2(&mut self, addr: u64) {
        unsafe {
            if self.enabled() && asan_giovese_store2(emu::g2h(addr)) != 0 {
                asan_giovese_report_and_crash(
                    1,
                    addr,
                    2,
                    emu::read_reg(Regs::Pc).unwrap_or(u64::MAX),
                    0,
                    emu::read_reg(Regs::Sp).unwrap_or(u64::MAX),
                );
            }
        }
    }

    pub fn write_4(&mut self, addr: u64) {
        unsafe {
            if self.enabled() && asan_giovese_store4(emu::g2h(addr)) != 0 {
                asan_giovese_report_and_crash(
                    1,
                    addr,
                    4,
                    emu::read_reg(Regs::Pc).unwrap_or(u64::MAX),
                    0,
                    emu::read_reg(Regs::Sp).unwrap_or(u64::MAX),
                );
            }
        }
    }

    pub fn write_8(&mut self, addr: u64) {
        unsafe {
            if self.enabled() && asan_giovese_store8(emu::g2h(addr)) != 0 {
                asan_giovese_report_and_crash(
                    1,
                    addr,
                    8,
                    emu::read_reg(Regs::Pc).unwrap_or(u64::MAX),
                    0,
                    emu::read_reg(Regs::Sp).unwrap_or(u64::MAX),
                );
            }
        }
    }

    pub fn write_n(&mut self, addr: u64, size: usize) {
        unsafe {
            if self.enabled() && asan_giovese_storeN(emu::g2h(addr), size) != 0 {
                asan_giovese_report_and_crash(
                    1,
                    addr,
                    size,
                    emu::read_reg(Regs::Pc).unwrap_or(u64::MAX),
                    0,
                    emu::read_reg(Regs::Sp).unwrap_or(u64::MAX),
                );
            }
        }
    }

    #[allow(clippy::unused_self)]
    pub fn poison(&mut self, addr: u64, size: usize, poison: PoisonKind) {
        unsafe { asan_giovese_poison_region(emu::g2h(addr), size, poison.into()) };
    }

    #[allow(clippy::unused_self)]
    pub fn unpoison(&mut self, addr: u64, size: usize) {
        unsafe { asan_giovese_unpoison_region(emu::g2h(addr), size) };
    }

    #[allow(clippy::unused_self)]
    pub fn reset(&mut self) {
        unsafe { asan_giovese_alloc_remove(0, u64::MAX) };
    }
}

impl Default for QemuAsanHelper {
    fn default() -> Self {
        Self::new()
    }
}

impl<I, S> QemuHelper<I, S> for QemuAsanHelper
where
    I: Input,
    S: HasMetadata,
{
    fn init<'a, H, OT, QT>(&self, executor: &QemuExecutor<'a, H, I, OT, QT, S>)
    where
        H: FnMut(&I) -> ExitKind,
        OT: ObserversTuple<I, S>,
        QT: QemuHelperTuple<I, S>,
    {
        //executor.hook_read_generation(gen_readwrite_asan::<I, QT, S>);
        executor.hook_read8_execution(trace_read8_asan::<I, QT, S>);
        executor.hook_read4_execution(trace_read4_asan::<I, QT, S>);
        executor.hook_read2_execution(trace_read2_asan::<I, QT, S>);
        executor.hook_read1_execution(trace_read1_asan::<I, QT, S>);
        executor.hook_read_n_execution(trace_read_n_asan::<I, QT, S>);

        //executor.hook_write_generation(gen_readwrite_asan::<I, QT, S>);
        executor.hook_write8_execution(trace_write8_asan::<I, QT, S>);
        executor.hook_write4_execution(trace_write4_asan::<I, QT, S>);
        executor.hook_write2_execution(trace_write2_asan::<I, QT, S>);
        executor.hook_write1_execution(trace_write1_asan::<I, QT, S>);
        executor.hook_write_n_execution(trace_write_n_asan::<I, QT, S>);

        executor.hook_syscalls(qasan_fake_syscall::<I, QT, S>);
    }

    fn post_exec(&mut self, _input: &I) {
        self.reset();
    }
}

// TODO add pc to generation hooks
pub fn gen_readwrite_asan<I, QT, S>(
    helpers: &mut QT,
    _state: &mut S,
    pc: u64,
    _size: usize,
) -> Option<u64>
where
    I: Input,
    QT: QemuHelperTuple<I, S>,
{
    let h = helpers.match_first_type_mut::<QemuAsanHelper>().unwrap();
    if h.must_instrument(pc) {
        Some(pc)
    } else {
        None
    }
}

pub fn trace_read1_asan<I, QT, S>(helpers: &mut QT, _state: &mut S, _id: u64, addr: u64)
where
    I: Input,
    QT: QemuHelperTuple<I, S>,
{
    let h = helpers.match_first_type_mut::<QemuAsanHelper>().unwrap();
    h.read_1(addr);
}

pub fn trace_read2_asan<I, QT, S>(helpers: &mut QT, _state: &mut S, _id: u64, addr: u64)
where
    I: Input,
    QT: QemuHelperTuple<I, S>,
{
    let h = helpers.match_first_type_mut::<QemuAsanHelper>().unwrap();
    h.read_2(addr);
}

pub fn trace_read4_asan<I, QT, S>(helpers: &mut QT, _state: &mut S, _id: u64, addr: u64)
where
    I: Input,
    QT: QemuHelperTuple<I, S>,
{
    let h = helpers.match_first_type_mut::<QemuAsanHelper>().unwrap();
    h.read_4(addr);
}

pub fn trace_read8_asan<I, QT, S>(helpers: &mut QT, _state: &mut S, _id: u64, addr: u64)
where
    I: Input,
    QT: QemuHelperTuple<I, S>,
{
    let h = helpers.match_first_type_mut::<QemuAsanHelper>().unwrap();
    h.read_8(addr);
}

pub fn trace_read_n_asan<I, QT, S>(
    helpers: &mut QT,
    _state: &mut S,
    _id: u64,
    addr: u64,
    size: usize,
) where
    I: Input,
    QT: QemuHelperTuple<I, S>,
{
    let h = helpers.match_first_type_mut::<QemuAsanHelper>().unwrap();
    h.read_n(addr, size);
}

pub fn trace_write1_asan<I, QT, S>(helpers: &mut QT, _state: &mut S, _id: u64, addr: u64)
where
    I: Input,
    QT: QemuHelperTuple<I, S>,
{
    let h = helpers.match_first_type_mut::<QemuAsanHelper>().unwrap();
    h.write_1(addr);
}

pub fn trace_write2_asan<I, QT, S>(helpers: &mut QT, _state: &mut S, _id: u64, addr: u64)
where
    I: Input,
    QT: QemuHelperTuple<I, S>,
{
    let h = helpers.match_first_type_mut::<QemuAsanHelper>().unwrap();
    h.write_2(addr);
}

pub fn trace_write4_asan<I, QT, S>(helpers: &mut QT, _state: &mut S, _id: u64, addr: u64)
where
    I: Input,
    QT: QemuHelperTuple<I, S>,
{
    let h = helpers.match_first_type_mut::<QemuAsanHelper>().unwrap();
    h.write_4(addr);
}

pub fn trace_write8_asan<I, QT, S>(helpers: &mut QT, _state: &mut S, _id: u64, addr: u64)
where
    I: Input,
    QT: QemuHelperTuple<I, S>,
{
    let h = helpers.match_first_type_mut::<QemuAsanHelper>().unwrap();
    h.write_8(addr);
}

pub fn trace_write_n_asan<I, QT, S>(
    helpers: &mut QT,
    _state: &mut S,
    _id: u64,
    addr: u64,
    size: usize,
) where
    I: Input,
    QT: QemuHelperTuple<I, S>,
{
    let h = helpers.match_first_type_mut::<QemuAsanHelper>().unwrap();
    h.read_n(addr, size);
}

pub fn qasan_fake_syscall<I, QT, S>(
    helpers: &mut QT,
    _state: &mut S,
    sys_num: i32,
    a0: u64,
    a1: u64,
    a2: u64,
    a3: u64,
    _a4: u64,
    _a5: u64,
    _a6: u64,
    _a7: u64,
) -> SyscallHookResult
where
    I: Input,
    QT: QemuHelperTuple<I, S>,
{
    if sys_num == QASAN_FAKESYS_NR {
        let h = helpers.match_first_type_mut::<QemuAsanHelper>().unwrap();
        let mut r = 0;
        match QasanAction::try_from(a0).expect("Invalid QASan action number") {
            QasanAction::CheckLoad => {
                h.read_n(a1, a2 as usize);
            }
            QasanAction::CheckStore => {
                h.write_n(a1, a2 as usize);
            }
            QasanAction::Poison => {
                h.poison(a1, a2 as usize, PoisonKind::try_from(a3 as u8).unwrap());
            }
            QasanAction::UserPoison => {
                h.poison(a1, a2 as usize, PoisonKind::User);
            }
            QasanAction::UnPoison => {
                h.unpoison(a1, a2 as usize);
            }
            QasanAction::IsPoison => {
                if h.is_poisoned(a1, a2 as usize) {
                    r = 1;
                }
            }
            QasanAction::Alloc => {
                h.alloc(a1, a2);
            }
            QasanAction::Dealloc => {
                h.dealloc(a1);
            }
            QasanAction::Enable => {
                h.set_enabled(true);
            }
            QasanAction::Disable => {
                h.set_enabled(false);
            }
            QasanAction::SwapState => {
                h.set_enabled(!h.enabled());
            }
        }
        SyscallHookResult::new(Some(r))
    } else {
        SyscallHookResult::new(None)
    }
}

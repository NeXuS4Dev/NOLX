//! Host-testable thread execution context, stack descriptors, TEB-equivalent metadata, and APCs.
//!
//! Architecture-specific switching code consumes [`CpuContext`] and the stable stack bounds;
//! this crate only defines and validates the data contract. APC queues are bounded so enqueue
//! failure is explicit instead of silently consuming kernel pool.

use crate::{ProcessId, ThreadId};

/// Why a stack descriptor could not be constructed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StackError {
    /// The base is not below the top.
    Empty,
    /// The stack bounds are not aligned to the architecture's 16-byte stack boundary.
    Misaligned,
    /// The stack is smaller than the minimum needed for an initial call frame.
    TooSmall,
    /// The top address overflowed while calculating the descriptor.
    AddressOverflow,
}

/// A half-open stack interval `[base, top)` with a separately reserved guard page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StackDescriptor {
    /// First usable virtual address.
    pub base: u64,
    /// One byte past the usable stack.
    pub top: u64,
    /// Address of the page immediately below `base`, if guarded.
    pub guard_page: Option<u64>,
}

impl StackDescriptor {
    /// Validate and create a descriptor from an aligned base and byte length.
    pub fn new(base: u64, length: u64, guard_page: Option<u64>) -> Result<Self, StackError> {
        if length == 0 {
            return Err(StackError::Empty);
        }
        if base & 0xF != 0 || length & 0xF != 0 {
            return Err(StackError::Misaligned);
        }
        if length < 512 {
            return Err(StackError::TooSmall);
        }
        let top = base
            .checked_add(length)
            .ok_or(StackError::AddressOverflow)?;
        if guard_page.is_some_and(|guard| guard >= base) {
            return Err(StackError::Misaligned);
        }
        Ok(Self {
            base,
            top,
            guard_page,
        })
    }

    /// Usable stack size in bytes.
    #[must_use]
    pub const fn length(self) -> u64 {
        self.top - self.base
    }
}

/// Integer and control-register state preserved by the architecture's switch routine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuContext {
    /// General-purpose registers in the architecture-defined saved order.
    pub general: [u64; 16],
    /// Instruction pointer.
    pub instruction_pointer: u64,
    /// Stack pointer.
    pub stack_pointer: u64,
    /// Processor flags with the reserved bit set.
    pub flags: u64,
    /// Address-space root (CR3 on x86-64).
    pub address_space_root: u64,
    /// FS base, used for thread-local storage.
    pub fs_base: u64,
    /// User GS base / TEB base.
    pub gs_base: u64,
    /// Optional pointer to a lazily allocated XSAVE/FPU save area.
    pub extended_state: Option<u64>,
    /// Whether this thread has used floating-point state since the last context switch.
    pub floating_point_used: bool,
}

impl CpuContext {
    /// A zeroed initial context with the architectural flags bit set.
    #[must_use]
    pub const fn initial() -> Self {
        Self {
            general: [0; 16],
            instruction_pointer: 0,
            stack_pointer: 0,
            flags: 1 << 1,
            address_space_root: 0,
            fs_base: 0,
            gs_base: 0,
            extended_state: None,
            floating_point_used: false,
        }
    }
}

/// The kind of APC routine to dispatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApcMode {
    /// A kernel APC, delivered by the kernel's APC-level path.
    Kernel,
    /// A user APC, delivered before returning to user mode.
    User,
}

/// A deferred routine queued to a thread's APC list.
#[derive(Clone, Copy)]
pub struct Apc {
    /// APC delivery mode.
    pub mode: ApcMode,
    /// The routine to call when the APC is delivered.
    pub routine: fn(usize),
    /// Opaque routine argument.
    pub argument: usize,
}

impl core::fmt::Debug for Apc {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("Apc")
            .field("mode", &self.mode)
            .field("argument", &self.argument)
            .finish_non_exhaustive()
    }
}

impl PartialEq for Apc {
    fn eq(&self, other: &Self) -> bool {
        self.mode == other.mode
            && self.routine as usize == other.routine as usize
            && self.argument == other.argument
    }
}

impl Eq for Apc {}

/// A bounded FIFO APC queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApcQueue<const CAPACITY: usize> {
    entries: [Option<Apc>; CAPACITY],
    length: usize,
}

impl<const CAPACITY: usize> ApcQueue<CAPACITY> {
    /// Create an empty APC queue.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: [None; CAPACITY],
            length: 0,
        }
    }

    /// Queue an APC, returning it unchanged if the fixed capacity is full.
    pub fn push(&mut self, apc: Apc) -> Result<(), Apc> {
        if self.length == CAPACITY {
            return Err(apc);
        }
        self.entries[self.length] = Some(apc);
        self.length += 1;
        Ok(())
    }

    /// Remove and return the oldest APC.
    pub fn pop(&mut self) -> Option<Apc> {
        if self.length == 0 {
            return None;
        }
        let first = self.entries[0].take();
        for index in 1..self.length {
            self.entries[index - 1] = self.entries[index].take();
        }
        self.length -= 1;
        first
    }

    /// Number of queued APCs.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.length
    }

    /// Whether the queue is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// Whether the queue has reached its fixed capacity.
    #[must_use]
    pub const fn is_full(&self) -> bool {
        self.length == CAPACITY
    }
}

impl<const CAPACITY: usize> Default for ApcQueue<CAPACITY> {
    fn default() -> Self {
        Self::new()
    }
}

/// A thread's execution state, ownership metadata, stacks, TEB-equivalent, and APC queues.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadContext<const APC_CAPACITY: usize> {
    /// Stable thread identity.
    pub id: ThreadId,
    /// Process that owns this thread.
    pub process: ProcessId,
    /// Saved integer/control-register state.
    pub cpu: CpuContext,
    /// Stable kernel stack bounds.
    pub kernel_stack: StackDescriptor,
    /// User stack bounds, absent for a kernel-only thread.
    pub user_stack: Option<StackDescriptor>,
    /// Address of the TEB-equivalent user metadata, not yet mapped before M6.
    pub teb_address: Option<u64>,
    /// APCs delivered while executing in kernel context.
    pub kernel_apcs: ApcQueue<APC_CAPACITY>,
    /// APCs delivered at the user-return boundary.
    pub user_apcs: ApcQueue<APC_CAPACITY>,
    /// Whether the thread is currently inside an alertable wait.
    pub alertable_wait: bool,
}

impl<const APC_CAPACITY: usize> ThreadContext<APC_CAPACITY> {
    /// Construct a kernel thread context with a validated stable kernel stack.
    #[must_use]
    pub const fn kernel(id: ThreadId, process: ProcessId, kernel_stack: StackDescriptor) -> Self {
        Self {
            id,
            process,
            cpu: CpuContext::initial(),
            kernel_stack,
            user_stack: None,
            teb_address: None,
            kernel_apcs: ApcQueue::new(),
            user_apcs: ApcQueue::new(),
            alertable_wait: false,
        }
    }

    /// Install user-mode stack and TEB-equivalent addresses before entering ring 3.
    pub fn set_user_state(&mut self, user_stack: StackDescriptor, teb_address: u64) {
        self.user_stack = Some(user_stack);
        self.teb_address = Some(teb_address);
        self.cpu.gs_base = teb_address;
    }

    /// Queue an APC in the list selected by its delivery mode.
    pub fn queue_apc(&mut self, apc: Apc) -> Result<(), Apc> {
        match apc.mode {
            ApcMode::Kernel => self.kernel_apcs.push(apc),
            ApcMode::User => self.user_apcs.push(apc),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_op(_: usize) {}

    #[test]
    fn stack_validation_checks_bounds_alignment_size_and_guard_location() {
        assert_eq!(
            StackDescriptor::new(0x1000, 0, None),
            Err(StackError::Empty)
        );
        assert_eq!(
            StackDescriptor::new(0x1001, 0x1000, None),
            Err(StackError::Misaligned)
        );
        assert_eq!(
            StackDescriptor::new(0x1000, 16, None),
            Err(StackError::TooSmall)
        );
        assert_eq!(
            StackDescriptor::new(0x1000, 0x1000, Some(0x1000)),
            Err(StackError::Misaligned)
        );
        let stack = StackDescriptor::new(0x2000, 0x4000, Some(0x1000)).unwrap();
        assert_eq!(stack.top, 0x6000);
        assert_eq!(stack.length(), 0x4000);
    }

    #[test]
    fn thread_keeps_kernel_user_stacks_teb_and_separate_apc_queues() {
        let kernel = StackDescriptor::new(0x1000, 0x2000, None).unwrap();
        let user = StackDescriptor::new(0x8000, 0x2000, Some(0x7000)).unwrap();
        let mut context = ThreadContext::<2>::kernel(ThreadId::new(9), ProcessId::new(4), kernel);
        assert_eq!(context.process, ProcessId::new(4));
        context.set_user_state(user, 0x9000);
        context
            .queue_apc(Apc {
                mode: ApcMode::Kernel,
                routine: no_op,
                argument: 1,
            })
            .unwrap();
        context
            .queue_apc(Apc {
                mode: ApcMode::User,
                routine: no_op,
                argument: 2,
            })
            .unwrap();
        assert_eq!(context.cpu.gs_base, 0x9000);
        assert_eq!(context.user_stack, Some(user));
        assert_eq!(context.kernel_apcs.len(), 1);
        assert_eq!(context.user_apcs.len(), 1);
    }

    #[test]
    fn apc_queue_is_bounded_and_fifo() {
        let mut queue = ApcQueue::<2>::new();
        let first = Apc {
            mode: ApcMode::Kernel,
            routine: no_op,
            argument: 1,
        };
        let second = Apc {
            mode: ApcMode::User,
            routine: no_op,
            argument: 2,
        };
        let third = Apc {
            mode: ApcMode::User,
            routine: no_op,
            argument: 3,
        };
        queue.push(first).unwrap();
        queue.push(second).unwrap();
        assert_eq!(queue.push(third).unwrap_err().argument, 3);
        assert_eq!(queue.pop().unwrap().argument, 1);
        queue.push(third).unwrap();
        assert_eq!(queue.pop().unwrap().argument, 2);
        assert_eq!(queue.pop().unwrap().argument, 3);
        assert!(queue.is_empty());
    }
}

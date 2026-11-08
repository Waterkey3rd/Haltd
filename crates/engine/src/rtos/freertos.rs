//! FreeRTOS task awareness. Every structure offset comes from DWARF.
//!
//! Reading from a running target can observe lists mid-update. Walks are
//! bounded and report such corruption as an error; halt for an exact snapshot.

use std::collections::BTreeMap;

use symbols::{
    exception_frame_size, CoreRegisters, MemoryReader, Result, SymbolDb, SymbolError, TypeKind,
    TypeRef,
};

const STACK_FILL_BYTE: u8 = 0xA5;
const STACK_SCAN_CHUNK: u32 = 256;
const MAX_STACK_SCAN: u32 = 1 << 20;
const MAX_LIST_ITEMS: usize = 256;
const TASK_WAITING_NOTIFICATION: u8 = 1;
const WORD: u32 = 4;

/// Which saved-context layout the firmware's port uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Port {
    /// `ARM_CM3`: R4-R11, then the hardware frame.
    NoFpu,
    /// `ARM_CM4F` and `ARM_CM7`: R4-R11, EXC_RETURN, optional S16-S31, then
    /// the hardware frame.
    Fpu,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TaskState {
    Running,
    Ready,
    Blocked,
    Suspended,
    Deleted,
}

impl TaskState {
    pub fn name(self) -> &'static str {
        match self {
            TaskState::Running => "running",
            TaskState::Ready => "ready",
            TaskState::Blocked => "blocked",
            TaskState::Suspended => "suspended",
            TaskState::Deleted => "deleted",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Task {
    pub tcb: u32,
    pub name: String,
    pub state: TaskState,
    pub priority: u32,
    /// Priority without mutex inheritance, when the firmware tracks it.
    pub base_priority: Option<u32>,
    /// Lowest address of the stack (`pxStack`).
    pub stack_base: u32,
    /// Saved stack pointer. Stale for the running task.
    pub top_of_stack: u32,
    /// Fill bytes never overwritten: `uxTaskGetStackHighWaterMark` × 4.
    pub stack_free_bytes: u32,
}

#[derive(Clone, Debug)]
pub struct TaskList {
    /// Running task first, then by descending priority.
    pub tasks: Vec<Task>,
    /// The kernel's own count, to compare with `tasks.len()`.
    pub kernel_task_count: u32,
}

#[derive(Clone, Copy, Debug)]
struct Offsets {
    top_of_stack: u32,
    event_list_item: u32,
    priority: u32,
    stack: u32,
    name: u32,
    name_len: u32,
    base_priority: Option<u32>,
    notify_state: Option<u32>,
    list_end: u32,
    mini_item_next: u32,
    item_next: u32,
    item_owner: u32,
    item_container: u32,
}

#[derive(Clone, Copy)]
struct KernelSymbols {
    current_tcb: u32,
    ready_lists: u32,
    ready_list_count: u32,
    list_size: u32,
    delayed: [u32; 2],
    pending_ready: u32,
    suspended: Option<u32>,
    terminating: Option<u32>,
    task_count: u32,
}

pub struct FreeRtos {
    port: Port,
    offsets: Offsets,
    symbols: KernelSymbols,
}

fn read_u32(memory: &mut dyn MemoryReader, addr: u32) -> Result<u32> {
    let mut bytes = [0u8; 4];
    memory.read(addr, &mut bytes).map_err(SymbolError::Memory)?;
    Ok(u32::from_le_bytes(bytes))
}

fn missing(what: &str) -> SymbolError {
    SymbolError::Type(format!("FreeRTOS: cannot find {what}"))
}

fn address_of(db: &SymbolDb, name: &str) -> Option<u32> {
    db.globals(name).first().map(|variable| variable.addr)
}

fn required_address(db: &SymbolDb, name: &str) -> Result<u32> {
    address_of(db, name).ok_or_else(|| missing(name))
}

fn type_of_global(db: &SymbolDb, name: &str) -> Result<TypeRef> {
    let variable = db
        .globals(name)
        .into_iter()
        .next()
        .ok_or_else(|| missing(name))?;
    db.variable_type(&variable)?.ok_or_else(|| missing(name))
}

fn member_offset(db: &SymbolDb, ty: TypeRef, name: &str) -> Result<Option<u32>> {
    Ok(db.find_member(ty, name)?.map(|(offset, _)| offset as u32))
}

fn required_member(db: &SymbolDb, ty: TypeRef, name: &str) -> Result<u32> {
    member_offset(db, ty, name)?.ok_or_else(|| missing(name))
}

/// The type a pointer refers to, looking through typedefs and qualifiers.
fn pointee(db: &SymbolDb, ty: TypeRef) -> Result<TypeRef> {
    let (_, info) = db.strip(ty)?;
    match info.kind {
        TypeKind::Pointer(Some(target)) => Ok(target),
        _ => Err(SymbolError::Type(format!(
            "FreeRTOS: expected a pointer, found {}",
            db.type_name(ty)
        ))),
    }
}

fn detect_port(db: &SymbolDb) -> Port {
    let uses = |path: &str| !db.source_files(path).is_empty();
    if uses("ARM_CM4F/port.c") || uses("ARM_CM7/r0p1/port.c") {
        Port::Fpu
    } else if uses("ARM_CM3/port.c") {
        Port::NoFpu
    } else {
        Port::Unknown
    }
}

impl FreeRtos {
    /// Returns None when the image does not contain FreeRTOS.
    pub fn detect(db: &SymbolDb) -> Result<Option<Self>> {
        if address_of(db, "pxCurrentTCB").is_none() || address_of(db, "pxReadyTasksLists").is_none()
        {
            return Ok(None);
        }

        let tcb = pointee(db, type_of_global(db, "pxCurrentTCB")?)?;
        let ready = type_of_global(db, "pxReadyTasksLists")?;
        let (_, ready_info) = db.strip(ready)?;
        let TypeKind::Array {
            element: Some(list),
            dimensions,
        } = &ready_info.kind
        else {
            return Err(SymbolError::Type(
                "FreeRTOS: pxReadyTasksLists is not an array".to_owned(),
            ));
        };
        let ready_list_count = dimensions
            .first()
            .copied()
            .flatten()
            .ok_or_else(|| missing("the size of pxReadyTasksLists"))?
            as u32;
        let list = *list;

        let (list_end, end_member) = db
            .find_member(list, "xListEnd")?
            .ok_or_else(|| missing("xListEnd"))?;
        let mini_item = end_member.ty.ok_or_else(|| missing("xListEnd's type"))?;
        let (mini_item_next, next_member) = db
            .find_member(mini_item, "pxNext")?
            .ok_or_else(|| missing("pxNext"))?;
        let item = pointee(db, next_member.ty.ok_or_else(|| missing("pxNext's type"))?)?;

        let (name, name_member) = db
            .find_member(tcb, "pcTaskName")?
            .ok_or_else(|| missing("pcTaskName"))?;
        let name_ty = name_member.ty.ok_or_else(|| missing("pcTaskName's type"))?;

        let offsets = Offsets {
            top_of_stack: required_member(db, tcb, "pxTopOfStack")?,
            event_list_item: required_member(db, tcb, "xEventListItem")?,
            priority: required_member(db, tcb, "uxPriority")?,
            stack: required_member(db, tcb, "pxStack")?,
            name: name as u32,
            name_len: db.size_of(name_ty, 0)? as u32,
            base_priority: member_offset(db, tcb, "uxBasePriority")?,
            notify_state: member_offset(db, tcb, "ucNotifyState")?,
            list_end: list_end as u32,
            mini_item_next: mini_item_next as u32,
            item_next: required_member(db, item, "pxNext")?,
            item_owner: required_member(db, item, "pvOwner")?,
            item_container: required_member(db, item, "pvContainer")?,
        };
        let symbols = KernelSymbols {
            current_tcb: required_address(db, "pxCurrentTCB")?,
            ready_lists: required_address(db, "pxReadyTasksLists")?,
            ready_list_count,
            list_size: db.size_of(list, 0)? as u32,
            delayed: [
                required_address(db, "xDelayedTaskList1")?,
                required_address(db, "xDelayedTaskList2")?,
            ],
            pending_ready: required_address(db, "xPendingReadyList")?,
            // These two exist only with the matching INCLUDE_ options.
            suspended: address_of(db, "xSuspendedTaskList"),
            terminating: address_of(db, "xTasksWaitingTermination"),
            task_count: required_address(db, "uxCurrentNumberOfTasks")?,
        };
        Ok(Some(Self {
            port: detect_port(db),
            offsets,
            symbols,
        }))
    }

    pub fn current_tcb(&self, memory: &mut dyn MemoryReader) -> Result<u32> {
        read_u32(memory, self.symbols.current_tcb)
    }

    pub fn port(&self) -> Port {
        self.port
    }

    pub fn priority_levels(&self) -> u32 {
        self.symbols.ready_list_count
    }

    /// TCB addresses on one list, in list order.
    fn walk_list(&self, list: u32, memory: &mut dyn MemoryReader) -> Result<Vec<u32>> {
        let end = list + self.offsets.list_end;
        let mut item = read_u32(memory, end + self.offsets.mini_item_next)?;
        let mut owners = Vec::new();
        for _ in 0..MAX_LIST_ITEMS {
            if item == end || item == 0 {
                return Ok(owners);
            }
            owners.push(read_u32(memory, item + self.offsets.item_owner)?);
            item = read_u32(memory, item + self.offsets.item_next)?;
        }
        Err(SymbolError::Type(format!(
            "FreeRTOS: list at {list:#010x} did not terminate; it is corrupt or was being modified"
        )))
    }

    /// A task on the suspended list may really be blocked with no timeout.
    fn suspended_list_state(&self, tcb: u32, memory: &mut dyn MemoryReader) -> Result<TaskState> {
        let offsets = &self.offsets;
        let container = read_u32(
            memory,
            tcb + offsets.event_list_item + offsets.item_container,
        )?;
        if container != 0 {
            return Ok(TaskState::Blocked);
        }
        if let Some(notify) = offsets.notify_state {
            let mut state = [0u8];
            memory
                .read(tcb + notify, &mut state)
                .map_err(SymbolError::Memory)?;
            if state[0] == TASK_WAITING_NOTIFICATION {
                return Ok(TaskState::Blocked);
            }
        }
        Ok(TaskState::Suspended)
    }

    /// Same scan as `uxTaskGetStackHighWaterMark`: stops at the first byte that
    /// is not the fill value, which may lie above the saved stack pointer when
    /// saved registers happen to hold fill bytes.
    fn stack_free_bytes(&self, stack_base: u32, memory: &mut dyn MemoryReader) -> Result<u32> {
        let mut chunk = [0u8; STACK_SCAN_CHUNK as usize];
        let mut address = stack_base;
        while address - stack_base < MAX_STACK_SCAN {
            // Aligned reads cannot run off the end of a RAM region.
            let length = (STACK_SCAN_CHUNK - address % STACK_SCAN_CHUNK) as usize;
            memory
                .read(address, &mut chunk[..length])
                .map_err(SymbolError::Memory)?;
            if let Some(used) = chunk[..length].iter().position(|b| *b != STACK_FILL_BYTE) {
                return Ok(address - stack_base + used as u32);
            }
            address += length as u32;
        }
        Err(SymbolError::Type(format!(
            "FreeRTOS: stack at {stack_base:#010x} has no end to its fill pattern"
        )))
    }

    fn read_task(&self, tcb: u32, state: TaskState, memory: &mut dyn MemoryReader) -> Result<Task> {
        let offsets = &self.offsets;
        let mut name = vec![0u8; offsets.name_len as usize];
        memory
            .read(tcb + offsets.name, &mut name)
            .map_err(SymbolError::Memory)?;
        let end = name.iter().position(|b| *b == 0).unwrap_or(name.len());
        let stack_base = read_u32(memory, tcb + offsets.stack)?;
        let top_of_stack = read_u32(memory, tcb + offsets.top_of_stack)?;
        let base_priority = match offsets.base_priority {
            Some(offset) => Some(read_u32(memory, tcb + offset)?),
            None => None,
        };
        Ok(Task {
            tcb,
            name: String::from_utf8_lossy(&name[..end]).into_owned(),
            state,
            priority: read_u32(memory, tcb + offsets.priority)?,
            base_priority,
            stack_base,
            top_of_stack,
            stack_free_bytes: self.stack_free_bytes(stack_base, memory)?,
        })
    }

    pub fn tasks(&self, memory: &mut dyn MemoryReader) -> Result<TaskList> {
        let symbols = &self.symbols;
        let current = read_u32(memory, symbols.current_tcb)?;
        let mut found: BTreeMap<u32, TaskState> = BTreeMap::new();
        for index in 0..symbols.ready_list_count {
            let list = symbols.ready_lists + index * symbols.list_size;
            for tcb in self.walk_list(list, memory)? {
                found.entry(tcb).or_insert(TaskState::Ready);
            }
        }
        for list in symbols.delayed {
            for tcb in self.walk_list(list, memory)? {
                found.entry(tcb).or_insert(TaskState::Blocked);
            }
        }
        for tcb in self.walk_list(symbols.pending_ready, memory)? {
            found.entry(tcb).or_insert(TaskState::Ready);
        }
        if let Some(list) = symbols.suspended {
            for tcb in self.walk_list(list, memory)? {
                let state = self.suspended_list_state(tcb, memory)?;
                found.entry(tcb).or_insert(state);
            }
        }
        if let Some(list) = symbols.terminating {
            for tcb in self.walk_list(list, memory)? {
                found.entry(tcb).or_insert(TaskState::Deleted);
            }
        }
        if current != 0 {
            found.insert(current, TaskState::Running);
        }

        let mut tasks = Vec::with_capacity(found.len());
        for (tcb, state) in found {
            tasks.push(self.read_task(tcb, state, memory)?);
        }
        tasks.sort_by_key(|task| {
            (
                task.state != TaskState::Running,
                std::cmp::Reverse(task.priority),
                task.tcb,
            )
        });
        Ok(TaskList {
            tasks,
            kernel_task_count: read_u32(memory, symbols.task_count)?,
        })
    }

    /// Registers of a task that is not running, from its saved context.
    pub fn saved_context(
        &self,
        task: &Task,
        memory: &mut dyn MemoryReader,
    ) -> Result<CoreRegisters> {
        let mut registers = CoreRegisters::default();
        let mut cursor = task.top_of_stack;
        for register in 4..=11 {
            registers.r[register] = read_u32(memory, cursor)?;
            cursor += WORD;
        }
        let exc_return = match self.port {
            Port::NoFpu => 0xFFFF_FFFD,
            Port::Fpu => {
                let value = read_u32(memory, cursor)?;
                cursor += WORD;
                if value & (1 << 4) == 0 {
                    cursor += 16 * WORD; // S16-S31
                }
                value
            }
            Port::Unknown => {
                return Err(SymbolError::Type(
                    "FreeRTOS: unrecognized port, cannot decode saved registers".to_owned(),
                ))
            }
        };
        const HARDWARE_ORDER: [usize; 7] = [0, 1, 2, 3, 12, 14, 15];
        for (slot, register) in HARDWARE_ORDER.into_iter().enumerate() {
            registers.r[register] = read_u32(memory, cursor + slot as u32 * WORD)?;
        }
        registers.xpsr = read_u32(memory, cursor + 7 * WORD)?;
        registers.r[13] = cursor + exception_frame_size(exc_return, registers.xpsr);
        registers.psp = registers.r[13];
        Ok(registers)
    }
}

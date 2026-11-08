//! Task awareness against hand-built kernel images; layouts come from DWARF
//! in real GCC output.
#![cfg(has_arm_gcc)]

use std::path::Path;

use engine::rtos::freertos::{FreeRtos, Port, TaskState};
use symbols::{LoadOptions, MemoryReader, SymbolDb};

const RAM: u32 = 0x2000_0000;
const RAM_SIZE: usize = 0x8000;
const TCB_SIZE: u32 = 76;
const ITEM_NEXT: u32 = 4;
const ITEM_OWNER: u32 = 12;
const ITEM_CONTAINER: u32 = 16;
const LIST_END: u32 = 8;
const TCB_STATE_ITEM: u32 = 4;
const TCB_EVENT_ITEM: u32 = 24;
const TCB_PRIORITY: u32 = 44;
const TCB_STACK: u32 = 48;
const TCB_NAME: u32 = 52;
const TCB_BASE_PRIORITY: u32 = 68;
const TCB_NOTIFY: u32 = 72;

struct Target {
    ram: Vec<u8>,
    next_tcb: u32,
}

impl MemoryReader for Target {
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), String> {
        let start = addr.wrapping_sub(RAM) as usize;
        let slice = self
            .ram
            .get(start..start + buf.len())
            .ok_or_else(|| format!("unmapped {addr:#x}"))?;
        buf.copy_from_slice(slice);
        Ok(())
    }
}

impl Target {
    /// Empty kernel lists, as vTaskStartScheduler leaves them.
    fn new(db: &SymbolDb) -> Self {
        let mut target = Self {
            ram: vec![0; RAM_SIZE],
            next_tcb: RAM + 0x2000,
        };
        for name in [
            "xDelayedTaskList1",
            "xDelayedTaskList2",
            "xPendingReadyList",
            "xSuspendedTaskList",
            "xTasksWaitingTermination",
        ] {
            target.empty_list(addr(db, name));
        }
        for index in 0..7 {
            target.empty_list(ready(db, index));
        }
        target
    }

    fn put(&mut self, addr: u32, value: u32) {
        let start = (addr - RAM) as usize;
        self.ram[start..start + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn get(&self, addr: u32) -> u32 {
        let start = (addr - RAM) as usize;
        u32::from_le_bytes(self.ram[start..start + 4].try_into().unwrap())
    }

    fn empty_list(&mut self, list: u32) {
        self.put(list + LIST_END + ITEM_NEXT, list + LIST_END);
    }

    /// Create a task with `free` fill bytes at the bottom of a 512-byte stack.
    fn task(&mut self, name: &str, priority: u32, free: u32) -> u32 {
        let tcb = self.next_tcb;
        self.next_tcb += TCB_SIZE + 512;
        let stack = tcb + TCB_SIZE;
        let top = stack + 512 - 64;
        self.put(tcb, top);
        self.put(tcb + TCB_PRIORITY, priority);
        self.put(tcb + TCB_BASE_PRIORITY, priority);
        self.put(tcb + TCB_STACK, stack);
        let name_at = (tcb + TCB_NAME - RAM) as usize;
        self.ram[name_at..name_at + name.len()].copy_from_slice(name.as_bytes());
        let stack_at = (stack - RAM) as usize;
        self.ram[stack_at..stack_at + free as usize].fill(0xA5);
        tcb
    }

    /// Append a task's state item to a list.
    fn link(&mut self, list: u32, tcb: u32) {
        let end = list + LIST_END;
        let item = tcb + TCB_STATE_ITEM;
        let mut last = end + ITEM_NEXT;
        while self.get(last) != end {
            last = self.get(last) + ITEM_NEXT;
        }
        self.put(last, item);
        self.put(item + ITEM_NEXT, end);
        self.put(item + ITEM_OWNER, tcb);
        self.put(item + ITEM_CONTAINER, list);
    }
}

fn load(path: &str) -> SymbolDb {
    SymbolDb::load(
        Path::new(path),
        &LoadOptions {
            no_cache: true,
            ..LoadOptions::default()
        },
    )
    .unwrap()
}

fn addr(db: &SymbolDb, name: &str) -> u32 {
    db.globals(name)[0].addr
}

fn ready(db: &SymbolDb, index: u32) -> u32 {
    addr(db, "pxReadyTasksLists") + index * 20
}

fn cm3() -> (SymbolDb, FreeRtos) {
    let db = load(env!("FIXTURE_CM3"));
    let rtos = FreeRtos::detect(&db).unwrap().expect("FreeRTOS");
    (db, rtos)
}

#[test]
fn detects_port_and_priority_levels() {
    let (_, rtos) = cm3();
    assert_eq!(rtos.port(), Port::NoFpu);
    assert_eq!(rtos.priority_levels(), 7);

    let db = load(env!("FIXTURE_CM4F"));
    let rtos = FreeRtos::detect(&db).unwrap().unwrap();
    assert_eq!(rtos.port(), Port::Fpu);
}

#[test]
fn classifies_every_list_and_orders_running_first() {
    let (db, rtos) = cm3();
    let mut t = Target::new(&db);

    let running = t.task("running", 2, 100);
    let ready_high = t.task("ready_hi", 5, 40);
    let delayed = t.task("delayed", 3, 8);
    let delayed2 = t.task("delayed2", 3, 12);
    let pending = t.task("pending", 1, 16);
    let forever = t.task("forever", 4, 20);
    let suspended = t.task("susp", 1, 24);
    let notified = t.task("notify", 1, 28);
    let deleted = t.task("gone", 0, 32);

    t.link(ready(&db, 2), running);
    t.link(ready(&db, 5), ready_high);
    t.link(addr(&db, "xDelayedTaskList1"), delayed);
    t.link(addr(&db, "xDelayedTaskList2"), delayed2);
    t.link(addr(&db, "xPendingReadyList"), pending);
    t.link(addr(&db, "xSuspendedTaskList"), forever);
    t.link(addr(&db, "xSuspendedTaskList"), suspended);
    t.link(addr(&db, "xSuspendedTaskList"), notified);
    t.link(addr(&db, "xTasksWaitingTermination"), deleted);
    t.put(forever + TCB_EVENT_ITEM + ITEM_CONTAINER, RAM + 0x100);
    let notify_at = (notified + TCB_NOTIFY - RAM) as usize;
    t.ram[notify_at] = 1;
    t.put(addr(&db, "pxCurrentTCB"), running);
    t.put(addr(&db, "uxCurrentNumberOfTasks"), 9);

    let list = rtos.tasks(&mut t).unwrap();
    assert_eq!(list.kernel_task_count, 9);
    assert_eq!(list.tasks.len(), 9);
    assert_eq!(list.tasks[0].name, "running");
    assert_eq!(list.tasks[1].name, "ready_hi");

    let state = |name: &str| {
        list.tasks
            .iter()
            .find(|task| task.name == name)
            .unwrap_or_else(|| panic!("{name} missing"))
            .state
    };
    assert_eq!(state("running"), TaskState::Running);
    assert_eq!(state("ready_hi"), TaskState::Ready);
    assert_eq!(state("delayed"), TaskState::Blocked);
    assert_eq!(state("delayed2"), TaskState::Blocked);
    assert_eq!(state("pending"), TaskState::Ready);
    assert_eq!(state("forever"), TaskState::Blocked);
    assert_eq!(state("susp"), TaskState::Suspended);
    assert_eq!(state("notify"), TaskState::Blocked);
    assert_eq!(state("gone"), TaskState::Deleted);

    assert_eq!(list.tasks[0].stack_free_bytes, 100);
    assert_eq!(list.tasks[0].base_priority, Some(2));
}

#[test]
fn stack_scan_crosses_chunk_boundaries_and_passes_the_saved_top() {
    let (db, rtos) = cm3();
    let mut t = Target::new(&db);
    let partly = t.task("partly", 1, 300);
    let untouched = t.task("full", 1, 512);
    t.link(ready(&db, 1), partly);
    t.link(ready(&db, 1), untouched);

    let list = rtos.tasks(&mut t).unwrap();
    let free = |name: &str| {
        list.tasks
            .iter()
            .find(|task| task.name == name)
            .unwrap()
            .stack_free_bytes
    };
    assert_eq!(free("partly"), 300);
    // The fill reaches the zeroed memory after this stack, past the saved SP.
    assert_eq!(free("full"), 512);
}

#[test]
fn a_cyclic_list_is_an_error_not_a_hang() {
    let (db, rtos) = cm3();
    let mut t = Target::new(&db);
    let task = t.task("loop", 1, 4);
    t.link(ready(&db, 1), task);
    let item = task + TCB_STATE_ITEM;
    t.put(item + ITEM_NEXT, item);
    let error = rtos.tasks(&mut t).unwrap_err().to_string();
    assert!(error.contains("did not terminate"), "{error}");
}

/// Write words at a task's saved stack pointer.
fn write_stack(t: &mut Target, tcb: u32, words: &[u32]) {
    let top = t.get(tcb);
    for (index, word) in words.iter().enumerate() {
        t.put(top + index as u32 * 4, *word);
    }
}

#[test]
fn decodes_a_cm3_saved_context() {
    let (db, rtos) = cm3();
    let mut t = Target::new(&db);
    let tcb = t.task("t", 1, 4);
    t.link(ready(&db, 1), tcb);
    let mut words: Vec<u32> = (104..=111).collect(); // R4-R11
    words.extend([100, 101, 102, 103, 112, 114, 0x0800_0100, 0x0100_0000]);
    write_stack(&mut t, tcb, &words);

    let task = rtos.tasks(&mut t).unwrap().tasks.remove(0);
    let registers = rtos.saved_context(&task, &mut t).unwrap();
    assert_eq!(registers.r[4], 104);
    assert_eq!(registers.r[11], 111);
    assert_eq!(registers.r[0], 100);
    assert_eq!(registers.r[12], 112);
    assert_eq!(registers.r[14], 114);
    assert_eq!(registers.r[15], 0x0800_0100);
    assert_eq!(registers.r[13], task.top_of_stack + 16 * 4);
}

#[test]
fn decodes_a_cm4f_context_with_fpu_frame_and_alignment_padding() {
    let db = load(env!("FIXTURE_CM4F"));
    let rtos = FreeRtos::detect(&db).unwrap().unwrap();
    let mut t = Target::new(&db);
    let tcb = t.task("t", 1, 4);
    t.link(ready(&db, 1), tcb);

    let mut words: Vec<u32> = (104..=111).collect();
    words.push(0xFFFF_FFED); // EXC_RETURN with FP context
    words.extend(std::iter::repeat_n(0, 16)); // S16-S31
    words.extend([100, 101, 102, 103, 112, 114, 0x0800_0200, 0x0100_0200]);
    write_stack(&mut t, tcb, &words);

    let task = rtos.tasks(&mut t).unwrap().tasks.remove(0);
    let registers = rtos.saved_context(&task, &mut t).unwrap();
    assert_eq!(registers.r[15], 0x0800_0200);
    assert_eq!(registers.r[0], 100);
    let hardware_frame = task.top_of_stack + 25 * 4;
    assert_eq!(registers.r[13], hardware_frame + 0x68 + 4);
}

#[test]
fn decodes_a_cm4f_context_without_fpu_use() {
    let db = load(env!("FIXTURE_CM4F"));
    let rtos = FreeRtos::detect(&db).unwrap().unwrap();
    let mut t = Target::new(&db);
    let tcb = t.task("t", 1, 4);
    t.link(ready(&db, 1), tcb);

    let mut words: Vec<u32> = (104..=111).collect();
    words.push(0xFFFF_FFFD);
    words.extend([100, 101, 102, 103, 112, 114, 0x0800_0300, 0x0100_0000]);
    write_stack(&mut t, tcb, &words);

    let task = rtos.tasks(&mut t).unwrap().tasks.remove(0);
    let registers = rtos.saved_context(&task, &mut t).unwrap();
    assert_eq!(registers.r[15], 0x0800_0300);
    assert_eq!(registers.r[13], task.top_of_stack + 9 * 4 + 0x20);
}

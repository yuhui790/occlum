use super::{ProcessRef, ThreadRef};
use crate::prelude::*;

pub fn get_process(pid: pid_t) -> Result<ProcessRef> {
    PROCESS_TABLE.lock().unwrap().get(pid)
}

pub(super) fn add_process(process: ProcessRef) -> Result<()> {
    PROCESS_TABLE.lock().unwrap().add(process.pid(), process)
}

pub(super) fn del_process(pid: pid_t) -> Result<ProcessRef> {
    PROCESS_TABLE.lock().unwrap().del(pid)
}

pub fn get_thread(tid: pid_t) -> Result<ThreadRef> {
    THREAD_TABLE.lock().unwrap().get(tid)
}

pub(super) fn add_thread(thread: ThreadRef) -> Result<()> {
    THREAD_TABLE.lock().unwrap().add(thread.tid(), thread)
}

pub(super) fn del_thread(tid: pid_t) -> Result<ThreadRef> {
    THREAD_TABLE.lock().unwrap().del(tid)
}

pub fn debug() {
    println!("process table = {:#?}", PROCESS_TABLE.lock().unwrap());
    println!("thread table = {:#?}", THREAD_TABLE.lock().unwrap());
    //println!("idle = {:#?}", *super::IDLE);
}

lazy_static! {
    static ref PROCESS_TABLE: SgxMutex<Table<ProcessRef>> =
        { SgxMutex::new(Table::<ProcessRef>::with_capacity(8)) };
    static ref THREAD_TABLE: SgxMutex<Table<ThreadRef>> =
        { SgxMutex::new(Table::<ThreadRef>::with_capacity(8)) };
}

#[derive(Debug, Clone)]
struct Table<I: Debug + Clone + Send + Sync> {
    map: HashMap<pid_t, I>,
}

impl<I: Debug + Clone + Send + Sync> Table<I> {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            map: HashMap::with_capacity(capacity),
        }
    }

    pub fn get(&self, id: pid_t) -> Result<I> {
        self.map
            .get(&id)
            .map(|item_ref| item_ref.clone())
            .ok_or_else(|| errno!(ESRCH, "id does not exist"))
    }

    pub fn add(&mut self, id: pid_t, item: I) -> Result<()> {
        if self.map.contains_key(&id) {
            return_errno!(EEXIST, "id is already added");
        }
        self.map.insert(id, item);
        Ok(())
    }

    pub fn del(&mut self, id: pid_t) -> Result<I> {
        if !self.map.contains_key(&id) {
            return_errno!(ENOENT, "id does not exist");
        }
        Ok(self.map.remove(&id).unwrap())
    }
}

/// Event types produced by the monitor threads and consumed by the policy engine.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProcessKind {
    Start,
    Stop,
}

#[derive(Clone, Debug)]
pub struct ProcessEvent {
    pub pid: u32,
    pub name: String,
    pub parent_pid: u32,
    pub kind: ProcessKind,
}

#[derive(Clone, Copy, Debug)]
pub struct ForegroundEvent {
    pub pid: u32,
}

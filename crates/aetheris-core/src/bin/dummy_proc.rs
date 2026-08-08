//! Test helper: spins forever so actions can be applied and observed.
fn main() {
    loop {
        std::hint::spin_loop();
    }
}

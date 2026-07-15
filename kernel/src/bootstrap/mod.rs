pub mod console;
#[cfg(any(test, all(target_os = "none", target_arch = "x86_64")))]
pub mod multiboot;
pub mod user;

//! Set the code pages used by a classic Windows console while the TUI runs.

#![allow(
    unsafe_code,
    reason = "Win32 console code-page calls have no safe standard-library wrapper"
)]

use std::io;

use windows_sys::Win32::System::Console::{
    GetConsoleCP, GetConsoleOutputCP, SetConsoleCP, SetConsoleOutputCP,
};

const UTF8: u32 = 65_001;

#[derive(Debug)]
pub struct Utf8Console {
    input: u32,
    output: u32,
}

impl Utf8Console {
    pub fn enter() -> io::Result<Self> {
        // SAFETY: both calls take no arguments and read this process's console.
        let (input, output) = unsafe { (GetConsoleCP(), GetConsoleOutputCP()) };
        if input == 0 || output == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: 65001 is the documented UTF-8 code page identifier.
        if input != UTF8 && unsafe { SetConsoleCP(UTF8) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: 65001 is the documented UTF-8 code page identifier.
        if output != UTF8 && unsafe { SetConsoleOutputCP(UTF8) } == 0 {
            let error = io::Error::last_os_error();
            if input != UTF8 {
                // SAFETY: `input` was read from this console above.
                unsafe { SetConsoleCP(input) };
            }
            return Err(error);
        }
        Ok(Self { input, output })
    }
}

impl Drop for Utf8Console {
    fn drop(&mut self) {
        if self.output != UTF8 {
            // SAFETY: `output` was read from this console on entry.
            unsafe { SetConsoleOutputCP(self.output) };
        }
        if self.input != UTF8 {
            // SAFETY: `input` was read from this console on entry.
            unsafe { SetConsoleCP(self.input) };
        }
    }
}

//! The two things the overlay asks of Windows: put text on the clipboard and
//! open a web page.

use anyhow::{Result, anyhow};
use windows::Win32::Foundation::{GlobalFree, HANDLE};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HSTRING, w};

pub fn copy_to_clipboard(text: &str) -> Result<()> {
    const CF_UNICODETEXT: u32 = 13;
    let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    unsafe {
        let memory = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2)?;
        let target = GlobalLock(memory) as *mut u16;
        if target.is_null() {
            return Err(anyhow!("could not lock clipboard memory"));
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), target, wide.len());
        let _ = GlobalUnlock(memory);
        if let Err(e) = OpenClipboard(None) {
            let _ = GlobalFree(Some(memory));
            return Err(e.into());
        }
        let stored = EmptyClipboard().and_then(|()| SetClipboardData(CF_UNICODETEXT, Some(HANDLE(memory.0))));
        let _ = CloseClipboard();
        // On success the clipboard owns the memory; otherwise it is still ours.
        if let Err(e) = stored {
            let _ = GlobalFree(Some(memory));
            return Err(e.into());
        }
        Ok(())
    }
}

/// Opens a web address in the default browser. Anything that is not an
/// https address is refused, so text from a server can never start a program.
pub fn open_in_browser(link: &str) -> Result<()> {
    if !link.starts_with("https://") {
        return Err(anyhow!("refusing to open a link that is not https"));
    }
    let result = unsafe { ShellExecuteW(None, w!("open"), &HSTRING::from(link), None, None, SW_SHOWNORMAL) };
    // ShellExecute reports success as a value above 32.
    if result.0 as usize > 32 { Ok(()) } else { Err(anyhow!("Windows could not open the browser")) }
}

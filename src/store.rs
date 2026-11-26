//! What Mestream keeps on disk between runs, all in one folder: the sign-in
//! token (encrypted with Windows DPAPI so only this Windows user on this
//! machine can read it) and a note of the streaming session that is open.

use anyhow::{Context, Result, anyhow};
use std::path::PathBuf;
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CryptProtectData, CryptUnprotectData,
};

/// Lives directly under the user profile, not under AppData: Windows redirects
/// AppData for processes started from packaged apps, so a token saved there by
/// one launcher would be invisible to a normal terminal.
pub fn dir() -> Result<PathBuf> {
    let base = std::env::var_os("USERPROFILE").context("USERPROFILE is not set")?;
    Ok(PathBuf::from(base).join(".mestream"))
}

fn session_path() -> Result<PathBuf> {
    Ok(dir()?.join("session.json"))
}

/// Remembers the open streaming session so a later launch can close it if
/// this one dies without the chance to.
pub fn save_session(base_uri: &str, path: &str) {
    let result = session_path().and_then(|file| {
        std::fs::create_dir_all(file.parent().unwrap())?;
        std::fs::write(file, serde_json::json!({ "baseUri": base_uri, "path": path }).to_string())?;
        Ok(())
    });
    if let Err(e) = result {
        eprintln!("[session] could not record the session: {e:#}");
    }
}

/// The session a previous launch left open, if any: (base URI, session path).
pub fn leftover_session() -> Option<(String, String)> {
    let text = std::fs::read_to_string(session_path().ok()?).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some((value["baseUri"].as_str()?.to_owned(), value["path"].as_str()?.to_owned()))
}

pub fn clear_session() {
    if let Ok(file) = session_path() {
        let _ = std::fs::remove_file(file);
    }
}

fn path() -> Result<PathBuf> {
    Ok(dir()?.join("auth.bin"))
}

/// Where earlier builds kept the token; read once to carry the sign-in over.
fn legacy_path() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(base).join("Mestream").join("auth.bin"))
}

fn dpapi(input: &[u8], protect: bool) -> Result<Vec<u8>> {
    let blob_in = CRYPT_INTEGER_BLOB {
        cbData: input.len() as u32,
        pbData: input.as_ptr() as *mut u8,
    };
    let mut blob_out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: blob_in points at `input` for the duration of the call; blob_out
    // is filled by the API with a LocalAlloc'd buffer that we copy and free.
    unsafe {
        if protect {
            CryptProtectData(&blob_in, None, None, None, None, 0, &mut blob_out)
        } else {
            CryptUnprotectData(&blob_in, None, None, None, None, 0, &mut blob_out)
        }
        .map_err(|e| anyhow!("DPAPI failed: {e}"))?;
        let out = std::slice::from_raw_parts(blob_out.pbData, blob_out.cbData as usize).to_vec();
        LocalFree(Some(HLOCAL(blob_out.pbData as *mut _)));
        Ok(out)
    }
}

pub fn save_refresh_token(token: &str) -> Result<()> {
    let path = path()?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, dpapi(token.as_bytes(), true)?)
        .with_context(|| format!("writing {}", path.display()))
}

pub fn load_refresh_token() -> Result<Option<String>> {
    let path = path()?;
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let Some(old) = legacy_path().and_then(|p| std::fs::read(p).ok()) else {
                return Ok(None);
            };
            let token = String::from_utf8(dpapi(&old, false)?)?;
            save_refresh_token(&token)?;
            return Ok(Some(token));
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    Ok(Some(String::from_utf8(dpapi(&data, false)?)?))
}

pub fn clear() -> Result<()> {
    if let Some(old) = legacy_path() {
        let _ = std::fs::remove_file(old);
    }
    match std::fs::remove_file(path()?) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

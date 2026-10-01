//! PAT persistence in the current Windows user's credential vault, not config.json.
//! The secret is never returned to the webview and diagnostics never include it.
const TARGET: &str = "net.meetrec.app/Callabo/PAT";

pub fn load() -> Result<Option<String>, String> {
    platform::read(TARGET)
}
pub fn save(token: &str) -> Result<(), String> {
    platform::write(TARGET, token)
}
pub fn forget() -> Result<(), String> {
    platform::delete(TARGET)
}

#[cfg(windows)]
mod platform {
    use windows::{
        core::{PCWSTR, PWSTR},
        Win32::{
            Foundation::ERROR_NOT_FOUND,
            Security::Credentials::{
                CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW,
                CRED_MAX_CREDENTIAL_BLOB_SIZE, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
            },
        },
    };
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }
    fn missing(e: &windows::core::Error) -> bool {
        e.code() == ERROR_NOT_FOUND.to_hresult()
    }

    pub(super) fn write(target: &str, token: &str) -> Result<(), String> {
        if token.is_empty() || token.len() > CRED_MAX_CREDENTIAL_BLOB_SIZE as usize {
            return Err("Invalid Callabo token length; nothing was saved.".into());
        }
        let mut name = wide(target);
        let mut username = wide("Callabo PAT");
        let mut bytes = token.as_bytes().to_vec();
        let credential = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: PWSTR(name.as_mut_ptr()),
            CredentialBlobSize: bytes.len() as u32,
            CredentialBlob: bytes.as_mut_ptr(),
            // Despite the name, this is current-user, local-machine persistence;
            // other users have their own credential sets, and it does not roam.
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            UserName: PWSTR(username.as_mut_ptr()),
            ..Default::default()
        };
        // SAFETY: all pointers reference live buffers throughout the synchronous call.
        let result = unsafe { CredWriteW(&credential, 0) };
        for byte in &mut bytes {
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
        result.map_err(|_| {
            "Cannot save PAT in Windows Credential Manager. No plaintext fallback was used.".into()
        })
    }

    pub(super) fn read(target: &str) -> Result<Option<String>, String> {
        let name = wide(target);
        let mut ptr = std::ptr::null_mut();
        // SAFETY: valid nul-terminated target and out-pointer. CredFree owns release.
        match unsafe { CredReadW(PCWSTR(name.as_ptr()), CRED_TYPE_GENERIC, 0, &mut ptr) } {
            Err(e) if missing(&e) => Ok(None),
            Err(_) => Err("Cannot read Windows Credential Manager.".into()),
            Ok(()) => {
                struct Credential(*mut CREDENTIALW);
                impl Drop for Credential {
                    fn drop(&mut self) {
                        unsafe { CredFree(self.0.cast()) };
                    }
                }
                let credential = Credential(ptr);
                if credential.0.is_null() {
                    return Err("Credential Manager returned an invalid credential.".into());
                }
                let credential = unsafe { &*credential.0 };
                if credential.CredentialBlobSize == 0
                    || credential.CredentialBlob.is_null()
                    || credential.CredentialBlobSize > CRED_MAX_CREDENTIAL_BLOB_SIZE
                {
                    return Err(
                        "Saved Callabo credential is invalid. Replace it in Settings.".into(),
                    );
                }
                let bytes = unsafe {
                    std::slice::from_raw_parts(
                        credential.CredentialBlob,
                        credential.CredentialBlobSize as usize,
                    )
                };
                String::from_utf8(bytes.to_vec())
                    .map(Some)
                    .map_err(|_| "Saved Callabo credential is invalid.".into())
            }
        }
    }

    pub(super) fn delete(target: &str) -> Result<(), String> {
        let name = wide(target);
        match unsafe { CredDeleteW(PCWSTR(name.as_ptr()), CRED_TYPE_GENERIC, 0) } {
            Ok(()) => Ok(()),
            Err(e) if missing(&e) => Ok(()),
            Err(_) => Err("Cannot remove PAT from Windows Credential Manager.".into()),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn vault_roundtrip_replace_and_delete_isolated_fake_credential() {
            struct TestCredential(String);
            impl Drop for TestCredential {
                fn drop(&mut self) {
                    let _ = delete(&self.0);
                }
            }
            let target = TestCredential(format!("net.meetrec.app/test/{}", uuid::Uuid::new_v4()));
            assert_eq!(read(&target.0).unwrap(), None);
            write(&target.0, "pat_fake_test_only").unwrap();
            assert_eq!(
                read(&target.0).unwrap().as_deref(),
                Some("pat_fake_test_only")
            );
            write(&target.0, "pat_replaced_test_only").unwrap();
            assert_eq!(
                read(&target.0).unwrap().as_deref(),
                Some("pat_replaced_test_only")
            );
            delete(&target.0).unwrap();
            assert_eq!(read(&target.0).unwrap(), None);
            delete(&target.0).unwrap();
        }
        #[test]
        fn rejects_empty_or_oversized_secrets_without_a_write() {
            assert!(write("unused-test-target", "").is_err());
            assert!(write("unused-test-target", &"x".repeat(2561)).is_err());
        }
    }
}

// This task targets Windows ARM64. Never substitute plaintext storage on other OSes.
#[cfg(not(windows))]
mod platform {
    pub(super) fn read(_: &str) -> Result<Option<String>, String> {
        Ok(None)
    }
    pub(super) fn write(_: &str, _: &str) -> Result<(), String> {
        Err("Secure Callabo token storage is currently supported on Windows only.".into())
    }
    pub(super) fn delete(_: &str) -> Result<(), String> {
        Ok(())
    }
}

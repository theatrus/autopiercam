use crate::protocol::Secret;
use anyhow::{Result, bail};
use sha2::{Digest, Sha256};

pub trait CredentialStore: Send + Sync {
    fn read(&self, target: &str) -> Result<Option<Secret>>;
    fn write(&self, target: &str, secret: &Secret) -> Result<()>;
    fn delete(&self, target: &str) -> Result<()>;
}

pub fn target(origin: &str, installation: &str) -> String {
    let digest: String = Sha256::digest(origin.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("AutoPierCam/Chatstronomy/{installation}/{digest}")
}

pub struct OsCredentialStore;

#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;
    #[test]
    #[ignore = "writes and removes one isolated synthetic Windows credential"]
    fn windows_credential_manager_round_trip() {
        let key = target(
            "https://synthetic-test.invalid",
            &crate::service::random_uuid().unwrap(),
        );
        struct Cleanup(String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = OsCredentialStore.delete(&self.0);
            }
        }
        let _cleanup = Cleanup(key.clone());
        assert!(OsCredentialStore.read(&key).unwrap().is_none());
        OsCredentialStore
            .write(
                &key,
                &Secret::new("csdc_synthetic_test_not_a_real_credential".into()),
            )
            .unwrap();
        assert_eq!(
            OsCredentialStore
                .read(&key)
                .unwrap()
                .unwrap()
                .expose_for_transport(),
            "csdc_synthetic_test_not_a_real_credential"
        );
        OsCredentialStore.delete(&key).unwrap();
        assert!(OsCredentialStore.read(&key).unwrap().is_none());
    }
}

#[cfg(target_os = "windows")]
impl CredentialStore for OsCredentialStore {
    fn read(&self, target: &str) -> Result<Option<Secret>> {
        use windows_sys::Win32::{Foundation::ERROR_NOT_FOUND, Security::Credentials::*};
        let target: Vec<u16> = target.encode_utf16().chain(Some(0)).collect();
        let mut credential = std::ptr::null_mut();
        // Windows owns this allocation; copy the blob before CredFree.
        unsafe {
            if CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) == 0 {
                if std::io::Error::last_os_error().raw_os_error() == Some(ERROR_NOT_FOUND as i32) {
                    return Ok(None);
                }
                bail!("Windows Credential Manager could not read the pairing");
            }
            let count = (*credential).CredentialBlobSize as usize;
            let result = if count == 0 || count > 4096 || (*credential).CredentialBlob.is_null() {
                Err(anyhow::anyhow!(
                    "Stored pairing is invalid; forget and pair again"
                ))
            } else {
                let bytes = std::slice::from_raw_parts((*credential).CredentialBlob, count);
                std::str::from_utf8(bytes)
                    .map(|v| Some(Secret::new(v.to_owned())))
                    .map_err(|_| {
                        anyhow::anyhow!("Stored pairing is invalid; forget and pair again")
                    })
            };
            CredFree(credential.cast());
            result
        }
    }

    fn write(&self, target: &str, secret: &Secret) -> Result<()> {
        use windows_sys::Win32::Security::Credentials::*;
        let mut target: Vec<u16> = target.encode_utf16().chain(Some(0)).collect();
        let bytes = secret.expose_for_transport().as_bytes();
        if bytes.is_empty() || bytes.len() > 4096 {
            bail!("Invalid pairing credential");
        }
        let credential = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: target.as_mut_ptr(),
            CredentialBlobSize: bytes.len() as u32,
            CredentialBlob: bytes.as_ptr().cast_mut(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            ..Default::default()
        };
        if unsafe { CredWriteW(&credential, 0) } == 0 {
            bail!("Windows Credential Manager could not save the pairing");
        }
        Ok(())
    }

    fn delete(&self, target: &str) -> Result<()> {
        use windows_sys::Win32::{Foundation::ERROR_NOT_FOUND, Security::Credentials::*};
        let target: Vec<u16> = target.encode_utf16().chain(Some(0)).collect();
        if unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) } == 0
            && std::io::Error::last_os_error().raw_os_error() != Some(ERROR_NOT_FOUND as i32)
        {
            bail!("Windows Credential Manager could not remove the pairing");
        }
        Ok(())
    }
}

#[cfg(not(target_os = "windows"))]
impl CredentialStore for OsCredentialStore {
    fn read(&self, _: &str) -> Result<Option<Secret>> {
        bail!("An OS credential store is required on this platform")
    }
    fn write(&self, _: &str, _: &Secret) -> Result<()> {
        bail!("An OS credential store is required on this platform")
    }
    fn delete(&self, _: &str) -> Result<()> {
        bail!("An OS credential store is required on this platform")
    }
}

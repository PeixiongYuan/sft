use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

const MODE_VAR: &str = "SFT_ASKPASS_MODE";
const FILE_VAR: &str = "SFT_ASKPASS_FILE";

#[derive(Debug, Clone)]
pub struct PasswordHandle {
    inner: Arc<PasswordFile>,
}

#[derive(Debug)]
struct PasswordFile {
    path: PathBuf,
}

impl PasswordHandle {
    pub fn new(password: &str) -> io::Result<Self> {
        let dir = askpass_dir()?;
        let path = unique_password_path(&dir);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        }

        file.write_all(password.as_bytes())?;
        file.flush()?;

        Ok(Self {
            inner: Arc::new(PasswordFile { path }),
        })
    }

    pub fn configure_command(&self, command: &mut Command) {
        if let Ok(exe) = std::env::current_exe() {
            command.env("SSH_ASKPASS", exe);
            command.env("SSH_ASKPASS_REQUIRE", "force");
            command.env(MODE_VAR, "1");
            command.env(FILE_VAR, &self.inner.path);
            command.env("DISPLAY", "sft:0");
        }
    }
}

impl Drop for PasswordFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub fn handle_askpass_if_requested() -> io::Result<bool> {
    if std::env::var_os(MODE_VAR).is_none() {
        return Ok(false);
    }

    let path = std::env::var_os(FILE_VAR)
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing askpass password file"))?;
    let password = fs::read_to_string(path)?;
    print!("{password}");
    Ok(true)
}

fn askpass_dir() -> io::Result<PathBuf> {
    let base = dirs::runtime_dir()
        .or_else(dirs::cache_dir)
        .or_else(|| dirs::home_dir().map(|home| home.join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("sft")
        .join("askpass");
    fs::create_dir_all(&base)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&base, fs::Permissions::from_mode(0o700))?;
    }

    Ok(base)
}

fn unique_password_path(dir: &std::path::Path) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    dir.join(format!("pw-{}-{nanos}", std::process::id()))
}

use std::{
    io,
    path::PathBuf,
    process::{Child, Command},
};

#[derive(Debug)]
pub enum FakeMevBoostRelayError {
    SpawnError,
    BinaryNotFound,
}

/// Helper struct to run a fake relay for testing.
/// It mainly runs a process and not much more.
/// Usage:
/// - FakeMevBoostRelay::new().spawn();
/// - Auto kill the child process when the returned FakeMevBoostRelayInstance gets dropped.
pub struct FakeMevBoostRelay {
    path: Option<PathBuf>,
}

impl Default for FakeMevBoostRelay {
    fn default() -> Self {
        Self::new()
    }
}

fn is_enabled() -> bool {
    match std::env::var("RUN_TEST_FAKE_MEV_BOOST_RELAY") {
        Ok(value) => value.to_lowercase() == "1",
        Err(_) => false,
    }
}

impl FakeMevBoostRelay {
    pub fn new() -> Self {
        Self { path: None }
    }

    pub fn spawn(self) -> Option<FakeMevBoostRelayInstance> {
        self.try_spawn().unwrap()
    }

    fn try_spawn(self) -> Result<Option<FakeMevBoostRelayInstance>, FakeMevBoostRelayError> {
        let mut cmd = if let Some(ref prg) = self.path {
            Command::new(prg)
        } else {
            Command::new("mev-boost-fake-relay")
        };
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit());

        match cmd.spawn() {
            Ok(child) => Ok(Some(FakeMevBoostRelayInstance { child })),
            Err(e) => match e.kind() {
                io::ErrorKind::NotFound => {
                    if is_enabled() {
                        // If the binary is not found but it is required, we should return an error
                        Err(FakeMevBoostRelayError::BinaryNotFound)
                    } else {
                        Ok(None)
                    }
                }
                _ => Err(FakeMevBoostRelayError::SpawnError),
            },
        }
    }
}

#[derive(Debug)]
pub struct FakeMevBoostRelayInstance {
    child: Child,
}

impl FakeMevBoostRelayInstance {
    pub fn endpoint(&self) -> String {
        "http://localhost:8080".to_string()
    }
}

impl Drop for FakeMevBoostRelayInstance {
    fn drop(&mut self) {
        self.child.kill().expect("could not kill mev-boost-server");
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[ignore]
    #[test]
    fn test_spawn_fake_mev_boost_server() {
        let srv = FakeMevBoostRelay::new().spawn();
        let _ = match srv {
            Some(srv) => srv,
            None => {
                println!("mev-boost binary not found, skipping test");
                return;
            }
        };
    }
}

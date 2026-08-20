//! Owner-authenticated credential reuse for the OpenSSH processes started by `vvssh`.

use std::collections::HashMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::{self, JoinHandle};

use vivido::binary::polling::{LocalListener, LocalStream};
use zeroize::Zeroizing;

const ENDPOINT_ENV: &str = "VVSSH_ASKPASS_ENDPOINT";
const CONTEXT_ENV: &str = "VVSSH_ASKPASS_CONTEXT";
const MAX_FIELD_LENGTH: usize = 16 * 1024;
const REQUEST_ANSWER: u8 = 1;
const REQUEST_SHUTDOWN: u8 = 2;

pub(super) struct CredentialBroker {
    endpoint: PathBuf,
    executable: PathBuf,
    worker: Option<JoinHandle<()>>,
}

impl CredentialBroker {
    pub(super) fn new(process_id: u32, nonce: u128) -> io::Result<Self> {
        let endpoint = endpoint_path(process_id, nonce);
        let listener = LocalListener::bind(&endpoint)?;
        listener.set_nonblocking(false)?;
        let executable = env::current_exe()?;
        let worker = thread::Builder::new().name("vvssh-askpass".into()).spawn(move || {
            let mut cache = CredentialCache::default();
            while let Ok(mut stream) = listener.accept() {
                match serve_request(&mut stream, &mut cache) {
                    Ok(true) => break,
                    Ok(false) => {},
                    Err(_) => {},
                }
            }
        })?;
        Ok(Self { endpoint, executable, worker: Some(worker) })
    }

    pub(super) fn command(&self, ssh: &OsStr, context: &str) -> Command {
        let mut command = Command::new(ssh);
        command
            .env("SSH_ASKPASS", &self.executable)
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env("DISPLAY", "vivido-askpass")
            .env(ENDPOINT_ENV, &self.endpoint)
            .env(CONTEXT_ENV, context);
        command
    }
}

impl Drop for CredentialBroker {
    fn drop(&mut self) {
        if let Ok(mut stream) = LocalStream::connect(&self.endpoint) {
            let _ = stream.write_all(&[REQUEST_SHUTDOWN]);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.endpoint);
    }
}

#[derive(Default)]
struct CredentialCache {
    answers: HashMap<String, CachedAnswer>,
}

struct CachedAnswer {
    value: Zeroizing<String>,
    source_context: String,
}

impl CredentialCache {
    fn answer(
        &mut self,
        context: &str,
        prompt: &str,
        read_answer: impl FnOnce() -> io::Result<String>,
    ) -> io::Result<Zeroizing<String>> {
        if is_reusable_prompt(prompt)
            && let Some(answer) = self.answers.get(prompt)
            && answer.source_context != context
        {
            return Ok(Zeroizing::new(answer.value.to_string()));
        }

        let answer = Zeroizing::new(read_answer()?);
        if answer.len() > MAX_FIELD_LENGTH {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "SSH answer is too long"));
        }
        if is_reusable_prompt(prompt) {
            self.answers.insert(
                prompt.to_owned(),
                CachedAnswer {
                    value: Zeroizing::new(answer.to_string()),
                    source_context: context.to_owned(),
                },
            );
        }
        Ok(answer)
    }
}

pub(super) fn is_helper() -> bool {
    env::var_os(ENDPOINT_ENV).is_some()
}

pub(super) fn run_helper() -> io::Result<()> {
    let endpoint = env::var_os(ENDPOINT_ENV)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing askpass endpoint"))?;
    let context = env::var(CONTEXT_ENV)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "missing askpass context"))?;
    let prompt = env::args_os().nth(1).unwrap_or_else(|| OsString::from("SSH password: "));
    let mut stream = LocalStream::connect(Path::new(&endpoint))?;
    stream.write_all(&[REQUEST_ANSWER])?;
    write_field(&mut stream, context.as_bytes())?;
    write_field(&mut stream, prompt.to_string_lossy().as_bytes())?;
    let answer = Zeroizing::new(read_field(&mut stream)?);
    io::stdout().write_all(&answer)?;
    io::stdout().write_all(b"\n")
}

fn serve_request(stream: &mut LocalStream, cache: &mut CredentialCache) -> io::Result<bool> {
    let mut kind = [0_u8; 1];
    stream.read_exact(&mut kind)?;
    if kind[0] == REQUEST_SHUTDOWN {
        return Ok(true);
    }
    if kind[0] != REQUEST_ANSWER {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid askpass request"));
    }
    let context = read_utf8_field(stream, "askpass context")?;
    let prompt = read_utf8_field(stream, "SSH prompt")?;
    let answer = cache.answer(&context, &prompt, || {
        rpassword::prompt_password(&prompt).map_err(io::Error::other)
    })?;
    write_field(stream, answer.as_bytes())?;
    Ok(false)
}

fn write_field(writer: &mut impl Write, value: &[u8]) -> io::Result<()> {
    let length = u32::try_from(value.len())
        .ok()
        .filter(|length| {
            usize::try_from(*length).ok().is_some_and(|length| length <= MAX_FIELD_LENGTH)
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "askpass field is too long"))?;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(value)
}

fn read_field(reader: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut encoded_length = [0_u8; 4];
    reader.read_exact(&mut encoded_length)?;
    let length = usize::try_from(u32::from_be_bytes(encoded_length))
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid askpass field length"))?;
    if length > MAX_FIELD_LENGTH {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "askpass field is too long"));
    }
    let mut value = vec![0_u8; length];
    reader.read_exact(&mut value)?;
    Ok(value)
}

fn read_utf8_field(reader: &mut impl Read, name: &str) -> io::Result<String> {
    String::from_utf8(read_field(reader)?).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, format!("{name} is not valid UTF-8"))
    })
}

fn is_reusable_prompt(prompt: &str) -> bool {
    let prompt = prompt.to_ascii_lowercase();
    prompt.contains("password") || prompt.contains("passphrase")
}

#[cfg(windows)]
fn endpoint_path(process_id: u32, nonce: u128) -> PathBuf {
    PathBuf::from(format!(r"\\.\pipe\vivido-vvssh-askpass-{process_id}-{nonce}"))
}

#[cfg(unix)]
fn endpoint_path(process_id: u32, nonce: u128) -> PathBuf {
    env::temp_dir().join(format!("vivido-vvssh-askpass-{process_id}-{nonce}.sock"))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn reuses_password_between_ssh_processes_but_reprompts_on_same_process_retry() {
        let reads = Cell::new(0);
        let mut cache = CredentialCache::default();
        let read = || {
            reads.set(reads.get() + 1);
            Ok(format!("secret-{}", reads.get()))
        };

        let setup = cache.answer("setup", "user@host's password: ", read).unwrap();
        let retry = cache.answer("setup", "user@host's password: ", read).unwrap();
        let media = cache.answer("media", "user@host's password: ", read).unwrap();
        let interactive = cache.answer("interactive", "user@host's password: ", read).unwrap();

        assert_eq!(&*setup, "secret-1");
        assert_eq!(&*retry, "secret-2");
        assert_eq!(&*media, "secret-2");
        assert_eq!(&*interactive, "secret-2");
        assert_eq!(reads.get(), 2);
    }

    #[test]
    fn does_not_reuse_challenge_responses() {
        let reads = Cell::new(0);
        let mut cache = CredentialCache::default();
        let read = || {
            reads.set(reads.get() + 1);
            Ok(reads.get().to_string())
        };

        cache.answer("setup", "Verification code: ", read).unwrap();
        cache.answer("media", "Verification code: ", read).unwrap();

        assert_eq!(reads.get(), 2);
    }

    #[test]
    fn rejects_oversized_protocol_fields_before_allocation() {
        let mut encoded = Vec::new();
        encoded.extend_from_slice(&u32::try_from(MAX_FIELD_LENGTH + 1).unwrap().to_be_bytes());
        assert_eq!(
            read_field(&mut encoded.as_slice()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}

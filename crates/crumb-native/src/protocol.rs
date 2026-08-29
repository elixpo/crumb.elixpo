//! Framing for command completion metadata emitted by native shell hooks.

use std::path::PathBuf;
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};

use crate::ShellKind;

const FRAME_START: u8 = 0x1e;
const FRAME_END: u8 = 0x1f;
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

/// Metadata collected when a native command completes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandCompletion {
    pub sequence: u64,
    pub exit_code: i32,
    pub cwd: PathBuf,
}

/// Visible terminal bytes and hidden lifecycle events decoded from one chunk.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DecodedChunk {
    pub visible: Vec<u8>,
    pub completions: Vec<CommandCompletion>,
}

/// Session-scoped encoder and incremental completion-frame decoder.
#[derive(Clone, Debug)]
pub struct CompletionProtocol {
    token: String,
    pending: Vec<u8>,
}

impl CompletionProtocol {
    /// Creates a protocol with a process-local, session-specific token.
    #[must_use]
    pub fn new() -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let counter = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
        Self::with_token(format!("{:x}{timestamp:x}{counter:x}", process::id()))
    }

    /// Creates a protocol with a fixed token for deterministic tests.
    ///
    /// # Panics
    ///
    /// Panics when `token` is empty or contains a non-ASCII hexadecimal byte.
    #[must_use]
    pub fn with_token(token: String) -> Self {
        assert!(
            !token.is_empty() && token.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "completion token must be non-empty ASCII hexadecimal"
        );
        Self {
            token,
            pending: Vec::new(),
        }
    }

    /// Builds a Bash submission that preserves shell state and emits hidden
    /// completion metadata on the following line.
    #[must_use]
    pub fn submission(&self, kind: ShellKind, command: &str, sequence: u64) -> String {
        match kind {
            ShellKind::Bash | ShellKind::Zsh => self.posix_submission(command, sequence),
            ShellKind::PowerShell => self.powershell_submission(command, sequence),
        }
    }

    /// Decodes arbitrary PTY chunks while retaining incomplete frame bytes.
    ///
    /// # Errors
    ///
    /// Returns an error when a matching lifecycle frame contains invalid
    /// numeric fields or malformed hexadecimal cwd bytes.
    pub fn decode(&mut self, input: &[u8]) -> Result<DecodedChunk> {
        self.pending.extend_from_slice(input);
        let prefix = self.frame_prefix();
        let mut decoded = DecodedChunk::default();

        loop {
            let Some(start) = find_subslice(&self.pending, &prefix) else {
                let retained = matching_suffix_len(&self.pending, &prefix);
                let visible_end = self.pending.len() - retained;
                decoded.visible.extend(self.pending.drain(..visible_end));
                break;
            };

            decoded.visible.extend(self.pending.drain(..start));
            let Some(relative_end) = self.pending.iter().position(|byte| *byte == FRAME_END) else {
                break;
            };

            let frame = self.pending[..relative_end].to_vec();
            self.pending.drain(..=relative_end);
            decoded.completions.push(self.parse_frame(&frame)?);
        }

        Ok(decoded)
    }

    fn frame_prefix(&self) -> Vec<u8> {
        format!("{}crumb:{}:", char::from(FRAME_START), self.token).into_bytes()
    }

    fn posix_submission(&self, command: &str, sequence: u64) -> String {
        format!(
            "{command}\n__crumb_status=$?; __crumb_cwd_hex=$(printf %s \"$PWD\" | od -An -tx1 | tr -d ' \\n'); printf '\\036crumb:{}:{sequence}:%s:%s\\037' \"$__crumb_status\" \"$__crumb_cwd_hex\"\n",
            self.token
        )
    }

    fn powershell_submission(&self, command: &str, sequence: u64) -> String {
        format!(
            "$global:LASTEXITCODE=$null; {command}\r\n$__crumb_status = if ($?) {{ 0 }} elseif ($null -ne $LASTEXITCODE) {{ $LASTEXITCODE }} else {{ 1 }}; $__crumb_cwd_hex = [Convert]::ToHexString([Text.Encoding]::UTF8.GetBytes((Get-Location).Path)).ToLowerInvariant(); [Console]::Write([char]0x1e + 'crumb:{}:{sequence}:' + $__crumb_status + ':' + $__crumb_cwd_hex + [char]0x1f)\r\n",
            self.token
        )
    }

    fn parse_frame(&self, frame: &[u8]) -> Result<CommandCompletion> {
        let text = std::str::from_utf8(frame)?;
        let payload = text
            .strip_prefix(&format!("{}crumb:{}:", char::from(FRAME_START), self.token))
            .ok_or_else(|| anyhow!("completion frame has an invalid session prefix"))?;
        let mut fields = payload.splitn(3, ':');
        let sequence = fields
            .next()
            .ok_or_else(|| anyhow!("completion frame is missing its sequence"))?
            .parse()?;
        let exit_code = fields
            .next()
            .ok_or_else(|| anyhow!("completion frame is missing its exit code"))?
            .parse()?;
        let cwd_hex = fields
            .next()
            .ok_or_else(|| anyhow!("completion frame is missing its cwd"))?;
        let cwd = PathBuf::from(String::from_utf8(decode_hex(cwd_hex)?)?);

        Ok(CommandCompletion {
            sequence,
            exit_code,
            cwd,
        })
    }
}

impl Default for CompletionProtocol {
    fn default() -> Self {
        Self::new()
    }
}

fn decode_hex(input: &str) -> Result<Vec<u8>> {
    if input.len() % 2 != 0 {
        return Err(anyhow!("cwd hex payload has an odd length"));
    }

    input
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair)?;
            Ok(u8::from_str_radix(pair, 16)?)
        })
        .collect()
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn matching_suffix_len(input: &[u8], prefix: &[u8]) -> usize {
    let maximum = input.len().min(prefix.len().saturating_sub(1));
    (1..=maximum)
        .rev()
        .find(|length| input.ends_with(&prefix[..*length]))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{CommandCompletion, CompletionProtocol};

    #[test]
    fn submission_runs_command_before_the_completion_hook() {
        let protocol = CompletionProtocol::with_token("abc123".to_owned());

        let submission = protocol.submission(crate::ShellKind::Bash, "cd /tmp", 7);

        assert!(submission.starts_with("cd /tmp\n__crumb_status=$?;"));
        assert!(submission.contains("crumb:abc123:7:"));
    }

    #[test]
    fn powershell_submission_resets_stale_native_exit_state() {
        let protocol = CompletionProtocol::with_token("abc123".to_owned());

        let submission =
            protocol.submission(crate::ShellKind::PowerShell, "Set-Location C:\\\\", 9);

        assert!(submission.starts_with("$global:LASTEXITCODE=$null;"));
        assert!(submission.contains("crumb:abc123:9:"));
    }

    #[test]
    fn decoder_hides_a_split_frame_and_preserves_visible_output() {
        let mut protocol = CompletionProtocol::with_token("abc123".to_owned());

        let first = protocol
            .decode(b"hello\r\n\x1ecrumb:abc")
            .expect("first chunk should decode");
        let second = protocol
            .decode(b"123:7:0:2f746d70\x1f")
            .expect("second chunk should decode");

        assert_eq!(first.visible, b"hello\r\n");
        assert!(first.completions.is_empty());
        assert!(second.visible.is_empty());
        assert_eq!(
            second.completions,
            [CommandCompletion {
                sequence: 7,
                exit_code: 0,
                cwd: PathBuf::from("/tmp"),
            }]
        );
    }

    #[test]
    fn unrelated_control_sequences_remain_visible() {
        let mut protocol = CompletionProtocol::with_token("abc123".to_owned());

        let decoded = protocol
            .decode(b"\x1b[31mred\x1b[0m")
            .expect("ANSI output should decode");

        assert_eq!(decoded.visible, b"\x1b[31mred\x1b[0m");
        assert!(decoded.completions.is_empty());
    }
}

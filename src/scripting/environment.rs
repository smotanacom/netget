//! Script runtime environment detection

use super::types::ScriptLanguage;
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;
use tracing::{debug, info, warn};

/// How long a single `<runtime> --version` probe may take before it is treated as absent.
///
/// `Command::output()` has no timeout: it reads the child's stdout to EOF, and if the child
/// never exits, the calling thread blocks **forever**. That is not theoretical here. A
/// `--test-threads=100` run wedged completely with 98 of 100 threads parked behind one
/// initialising `AppState::new`, and the initialiser's stack was
/// `detect → detect_javascript → Command::output → read_output`. In production the same stack
/// hangs `netget --mcp` at startup, before it can report anything, if `node` or `python3` is
/// wedged on the machine.
///
/// Five seconds is far more than a version probe needs and short enough that an operator sees
/// a startup, not a hang.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Detected once per process. The answer is a property of the MACHINE, not of an `AppState`.
///
/// `AppState::new()` used to run all four probes, and every test and every server creation
/// calls it — so a 100-thread suite spawned four hundred subprocesses to learn the same four
/// facts, and any one of them hanging stalled everything behind it.
static DETECTED: OnceLock<ScriptingEnvironment> = OnceLock::new();

/// Information about available scripting environments
#[derive(Debug, Clone, Default)]
pub struct ScriptingEnvironment {
    /// Python availability and version
    pub python: Option<String>,

    /// JavaScript (Node.js) availability and version
    pub javascript: Option<String>,

    /// Go availability and version
    pub go: Option<String>,

    /// Perl availability and version
    pub perl: Option<String>,
}

impl ScriptingEnvironment {
    /// Detect available scripting environments
    /// The process-wide detection result, computed on first call.
    ///
    /// Prefer this over [`Self::detect_uncached`] everywhere: runtimes do not appear and
    /// disappear while NetGet runs, and probing per `AppState` costs four subprocess spawns
    /// each time for an answer that cannot have changed.
    pub fn detect() -> Self {
        DETECTED.get_or_init(Self::detect_uncached).clone()
    }

    /// Run the probes. Public for the one caller that genuinely wants to re-probe.
    pub fn detect_uncached() -> Self {
        debug!("Detecting scripting environments...");
        debug!("Detecting Python...");
        let python = Self::detect_python();
        debug!("Python detection complete");

        debug!("Detecting JavaScript/Node.js...");
        let javascript = Self::detect_javascript();
        debug!("JavaScript detection complete");

        debug!("Detecting Go...");
        let go = Self::detect_go();
        debug!("Go detection complete");

        debug!("Detecting Perl...");
        let perl = Self::detect_perl();
        debug!("Perl detection complete");

        info!("Scripting environment detection:");
        if let Some(ref ver) = python {
            info!("  Python: {} ✓", ver);
        } else {
            info!("  Python: not available");
        }
        if let Some(ref ver) = javascript {
            info!("  Node.js: {} ✓", ver);
        } else {
            info!("  Node.js: not available");
        }
        if let Some(ref ver) = go {
            info!("  Go: {} ✓", ver);
        } else {
            info!("  Go: not available");
        }
        if let Some(ref ver) = perl {
            info!("  Perl: {} ✓", ver);
        } else {
            info!("  Perl: not available");
        }

        Self {
            python,
            javascript,
            go,
            perl,
        }
    }

    /// Run `<program> --version` with a deadline, returning its trimmed output.
    ///
    /// The child is killed if it outlives [`PROBE_TIMEOUT`], because the alternative is
    /// `Command::output()`'s behaviour: block on the stdout pipe until the child exits, with
    /// no way out.
    pub fn probe(program: &str, args: &[&str]) -> Option<String> {
        use std::process::Stdio;

        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| debug!("{program} not found: {e}"))
            .ok()?;

        let deadline = std::time::Instant::now() + PROBE_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let out = child.wait_with_output().ok()?;
                    if !status.success() {
                        debug!("{program} probe failed: {status:?}");
                        return None;
                    }
                    // Go writes its version to stdout; some runtimes use stderr.
                    let text = if out.stdout.is_empty() {
                        String::from_utf8_lossy(&out.stderr)
                    } else {
                        String::from_utf8_lossy(&out.stdout)
                    };
                    let text = text.trim().to_string();
                    return (!text.is_empty()).then_some(text);
                }
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        warn!(
                            "{program} did not answer --version within {PROBE_TIMEOUT:?}; \
                             treating it as unavailable. A hung runtime must not hold up \
                             startup."
                        );
                        let _ = child.kill();
                        let _ = child.wait();
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => {
                    debug!("{program} probe error: {e}");
                    return None;
                }
            }
        }
    }

    fn detect_python() -> Option<String> {
        Self::probe("python3", &["--version"]).inspect(|v| debug!("python3 detected: {v}"))
    }

    /// Detect Node.js availability and version
    fn detect_javascript() -> Option<String> {
        Self::probe("node", &["--version"]).inspect(|v| debug!("node detected: {v}"))
    }

    /// Detect Go availability and version
    fn detect_go() -> Option<String> {
        Self::probe("go", &["version"]).inspect(|v| debug!("go detected: {v}"))
    }

    /// Detect Perl availability and version
    fn detect_perl() -> Option<String> {
        Self::probe("perl", &["--version"]).inspect(|v| debug!("perl detected: {v}"))
    }

    /// Check if a specific language is available
    pub fn is_available(&self, language: ScriptLanguage) -> bool {
        match language {
            ScriptLanguage::Python => self.python.is_some(),
            ScriptLanguage::JavaScript => self.javascript.is_some(),
            ScriptLanguage::Go => self.go.is_some(),
            ScriptLanguage::Perl => self.perl.is_some(),
        }
    }

    /// Get version string for a language
    pub fn get_version(&self, language: ScriptLanguage) -> Option<&str> {
        match language {
            ScriptLanguage::Python => self.python.as_deref(),
            ScriptLanguage::JavaScript => self.javascript.as_deref(),
            ScriptLanguage::Go => self.go.as_deref(),
            ScriptLanguage::Perl => self.perl.as_deref(),
        }
    }

    /// Format available environments for display to user/LLM
    pub fn format_available(&self) -> String {
        let mut parts = Vec::new();

        if let Some(ref ver) = self.python {
            parts.push(format!("Python ({})", ver));
        }
        if let Some(ref ver) = self.javascript {
            parts.push(format!("Node.js ({})", ver));
        }
        if let Some(ref ver) = self.go {
            parts.push(format!("Go ({})", ver));
        }
        if let Some(ref ver) = self.perl {
            parts.push(format!("Perl ({})", ver));
        }

        if parts.is_empty() {
            "None".to_string()
        } else {
            parts.join(", ")
        }
    }

    /// Warn if language is not available
    pub fn warn_if_unavailable(&self, language: ScriptLanguage) {
        if !self.is_available(language) {
            warn!(
                "{} is not available on this system. Scripts using {} will fail.",
                language.as_str(),
                language.as_str()
            );
        }
    }
}

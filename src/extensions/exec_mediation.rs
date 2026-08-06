//! Dangerous-command mediation and secret-broker policy.

use super::{DangerousCommandClass, ExecMediationPolicy, ExecRiskTier, SecretBrokerPolicy};

// ---------------------------------------------------------------------------
// Exec mediation and secret broker (SEC-4.3 / bd-zh0hj)
// ---------------------------------------------------------------------------

impl DangerousCommandClass {
    /// Human-readable label for incident logging.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::RecursiveDelete => "recursive_delete",
            Self::DeviceWrite => "device_write",
            Self::ForkBomb => "fork_bomb",
            Self::PipeToShell => "pipe_to_shell",
            Self::SystemShutdown => "system_shutdown",
            Self::PermissionEscalation => "permission_escalation",
            Self::ProcessTermination => "process_termination",
            Self::CredentialFileModification => "credential_file_modification",
            Self::DiskWipe => "disk_wipe",
            Self::ReverseShell => "reverse_shell",
        }
    }

    /// Risk tier for this command class (used for policy decisions).
    #[must_use]
    pub const fn risk_tier(self) -> ExecRiskTier {
        match self {
            Self::RecursiveDelete
            | Self::DeviceWrite
            | Self::ForkBomb
            | Self::DiskWipe
            | Self::ReverseShell => ExecRiskTier::Critical,
            Self::PipeToShell
            | Self::SystemShutdown
            | Self::PermissionEscalation
            | Self::ProcessTermination
            | Self::CredentialFileModification => ExecRiskTier::High,
        }
    }
}

impl Default for ExecMediationPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            deny_threshold: ExecRiskTier::Critical,
            deny_patterns: Vec::new(),
            allow_patterns: Vec::new(),
            audit_all_classified: true,
        }
    }
}

impl ExecMediationPolicy {
    /// Strict preset: blocks High and above.
    #[must_use]
    pub const fn strict() -> Self {
        Self {
            enabled: true,
            deny_threshold: ExecRiskTier::High,
            deny_patterns: Vec::new(),
            allow_patterns: Vec::new(),
            audit_all_classified: true,
        }
    }

    /// Permissive preset: only blocks Critical.
    #[must_use]
    pub const fn permissive() -> Self {
        Self {
            enabled: true,
            deny_threshold: ExecRiskTier::Critical,
            deny_patterns: Vec::new(),
            allow_patterns: Vec::new(),
            audit_all_classified: false,
        }
    }

    /// Disabled preset: no exec mediation.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            deny_threshold: ExecRiskTier::Critical,
            deny_patterns: Vec::new(),
            allow_patterns: Vec::new(),
            audit_all_classified: false,
        }
    }
}

pub(super) fn normalize_command_for_classification(command: &str) -> String {
    let mut normalized = String::with_capacity(command.len());
    let mut previous_was_space = false;
    let mut remaining = command;

    while !remaining.is_empty() {
        // Normalize common shell-obfuscated spacing forms that still evaluate
        // to whitespace at runtime.
        if let Some(rest) = remaining.strip_prefix("${ifs}") {
            if !previous_was_space {
                normalized.push(' ');
                previous_was_space = true;
            }
            remaining = rest;
            continue;
        }
        if let Some(rest) = remaining.strip_prefix("$ifs") {
            if !previous_was_space {
                normalized.push(' ');
                previous_was_space = true;
            }
            remaining = rest;
            continue;
        }

        let mut chars = remaining.chars();
        let Some(mut ch) = chars.next() else {
            break;
        };

        // Strip quotes to prevent obfuscation like `r"m" -rf /`
        if ch == '\'' || ch == '"' {
            remaining = chars.as_str();
            continue;
        }

        if ch == '\\' {
            let mut peek_chars = chars.clone();
            if let Some(next) = peek_chars.next() {
                if next == '\n' || next == '\r' {
                    remaining = peek_chars.as_str();
                    continue;
                }

                chars.next(); // consume the escaped character

                if next.is_ascii_whitespace() {
                    if !previous_was_space {
                        normalized.push(' ');
                        previous_was_space = true;
                    }
                    remaining = chars.as_str();
                    continue;
                }

                // Strip escaped quotes as well
                if next == '\'' || next == '"' {
                    remaining = chars.as_str();
                    continue;
                }

                ch = next;
            }
        }

        if ch.is_ascii_whitespace() {
            if !previous_was_space {
                normalized.push(' ');
                previous_was_space = true;
            }
        } else {
            normalized.push(ch);
            previous_was_space = false;
        }
        remaining = chars.as_str();
    }

    normalized
}

pub(super) fn classify_recursive_delete(lower: &str) -> bool {
    // rm -rf / or rm -rf /* or rm -rf ~
    if !lower.contains("rm") {
        return false;
    }
    // Detect recursive+force in any combination: -rf, -fr, --recursive,
    // or separate flags like -r -f / -f -r.
    let has_rf = lower.contains("-rf")
        || lower.contains("-fr")
        || lower.contains("--recursive")
        || (lower.contains("-r") && lower.contains("-f"));
    if !has_rf {
        return false;
    }
    // Target root, home, or wildcard
    let dangerous_targets = [" /", " /*", " /.", " ~/", " ~/*", " --no-preserve-root"];
    dangerous_targets.iter().any(|t| lower.contains(t))
}

pub(super) fn classify_device_write(lower: &str) -> bool {
    // dd writing to devices, mkfs, fdisk
    let dd_to_dev = lower.contains("dd ") && lower.contains("of=/dev/");
    let mkfs = lower.starts_with("mkfs") || lower.contains(" mkfs") || lower.contains(";mkfs");
    let fdisk = lower.starts_with("fdisk") || lower.contains(" fdisk") || lower.contains(";fdisk");
    dd_to_dev || mkfs || fdisk
}

pub(super) fn classify_fork_bomb(lower: &str) -> bool {
    // Classic bash fork bomb: :(){ :|:& };:
    // Also: while true; do ... & done
    lower.contains(":(){ :|:&")
        || lower.contains(":(){ :|: &")
        || (lower.contains("while true") && lower.contains("& done"))
        || (lower.contains("fork") && lower.contains("while") && lower.contains('&'))
}

pub(super) fn classify_disk_wipe(lower: &str) -> bool {
    let shred = lower.starts_with("shred") || lower.contains(" shred ") || lower.contains(";shred");
    let wipefs =
        lower.starts_with("wipefs") || lower.contains(" wipefs") || lower.contains(";wipefs");
    let dd_zero = lower.contains("dd ") && lower.contains("if=/dev/zero");
    let dd_urandom = lower.contains("dd ") && lower.contains("if=/dev/urandom");
    shred || wipefs || dd_zero || dd_urandom
}

pub(super) fn classify_reverse_shell(lower: &str) -> bool {
    // Common reverse shell patterns
    let bash_rev = lower.contains("/dev/tcp/") && lower.contains("bash");
    let nc_rev = (lower.contains("nc ") || lower.contains("ncat ") || lower.contains("netcat "))
        && lower.contains("-e ");
    let python_rev = lower.contains("socket") && lower.contains("connect") && lower.contains("sh");
    bash_rev || nc_rev || python_rev
}

pub(super) fn classify_pipe_to_shell(lower: &str) -> bool {
    // curl/wget piped to sh/bash. Cover bare `sh`/`bash` plus the absolute
    // paths the shell may live at on the target system: `/bin/{sh,bash}`
    // (Linux base layout), `/usr/bin/{sh,bash}` (/usr-merge distros — Arch,
    // Fedora, modern Debian/Ubuntu — where /bin is a symlink to /usr/bin),
    // and `/usr/local/bin/{sh,bash}` (FreeBSD `pkg install bash`, custom
    // installs). Both spaced and unspaced pipe forms.
    const PIPE_SHELL_PATTERNS: &[&str] = &[
        "| sh",
        "| bash",
        "|sh",
        "|bash",
        "| /bin/sh",
        "| /bin/bash",
        "|/bin/sh",
        "|/bin/bash",
        "| /usr/bin/sh",
        "| /usr/bin/bash",
        "|/usr/bin/sh",
        "|/usr/bin/bash",
        "| /usr/local/bin/sh",
        "| /usr/local/bin/bash",
        "|/usr/local/bin/sh",
        "|/usr/local/bin/bash",
    ];
    let has_download = lower.contains("curl ") || lower.contains("wget ");
    let has_pipe_to_shell = PIPE_SHELL_PATTERNS.iter().any(|p| lower.contains(p));
    let download_exec_patterns = [
        "eval \"$(curl ",
        "eval \"$(wget ",
        "eval '$(curl ",
        "eval '$(wget ",
        "eval $(curl ",
        "eval $(wget ",
        "source <(curl ",
        "source <(wget ",
        "bash -c \"$(curl ",
        "bash -c \"$(wget ",
        "bash -c '$(curl ",
        "bash -c '$(wget ",
        "sh -c \"$(curl ",
        "sh -c \"$(wget ",
        "sh -c '$(curl ",
        "sh -c '$(wget ",
    ];
    (has_download && has_pipe_to_shell)
        || download_exec_patterns
            .iter()
            .any(|pattern| lower.contains(pattern))
}

pub(super) fn classify_system_shutdown(lower: &str) -> bool {
    lower.starts_with("shutdown")
        || lower.contains(" shutdown")
        || lower.contains(";shutdown")
        || lower.starts_with("reboot")
        || lower.contains(" reboot")
        || lower.contains(";reboot")
        || lower.starts_with("halt")
        || lower.contains(" halt")
        || lower.contains(";halt")
        || lower.starts_with("poweroff")
        || lower.contains(" poweroff")
        || lower.contains(";poweroff")
        || lower.starts_with("init 0")
        || lower.contains(" init 0")
        || lower.starts_with("init 6")
        || lower.contains(" init 6")
}

pub(super) fn classify_permission_escalation(lower: &str) -> bool {
    // chmod 777, chmod -R 777, chown root
    let chmod_broad = lower.contains("chmod")
        && (lower.contains("777") || lower.contains("a+rwx") || lower.contains("o+w"));
    let chmod_suid = lower.contains("chmod") && (lower.contains("+s") || lower.contains("4755"));
    chmod_broad || chmod_suid
}

pub(super) fn classify_process_termination(lower: &str) -> bool {
    // kill -9 1, pkill init, killall
    let kill_pid1 = lower.contains("kill") && (lower.contains(" 1 ") || lower.ends_with(" 1"));
    let kill_9 = lower.contains("kill -9") || lower.contains("kill -kill");
    let pkill_critical = lower.contains("pkill")
        && (lower.contains("init") || lower.contains("systemd") || lower.contains("sshd"));
    let killall = lower.starts_with("killall") || lower.contains(" killall");
    // Only flag kill of PID 1 or critical processes
    (kill_pid1 && kill_9) || pkill_critical || killall
}

pub(super) fn classify_credential_file_modification(lower: &str) -> bool {
    let cred_files = [
        "/etc/passwd",
        "/etc/shadow",
        "/etc/sudoers",
        "/etc/ssh/sshd_config",
    ];
    let write_cmds = ["tee ", "cat >", "echo >", "sed -i", "cp ", "mv "];
    cred_files
        .iter()
        .any(|f| lower.contains(f) && write_cmds.iter().any(|w| lower.contains(w)))
}

impl ExecRiskTier {
    /// Human-readable label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }
}

// ---------------------------------------------------------------------------
// Secret broker (SEC-4.3 / bd-zh0hj)
// ---------------------------------------------------------------------------

impl Default for SecretBrokerPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            secret_suffixes: vec![
                "_KEY".to_string(),
                "_SECRET".to_string(),
                "_TOKEN".to_string(),
                "_PASSWORD".to_string(),
                "_PASSWD".to_string(),
                "_CREDENTIAL".to_string(),
                "_CREDENTIALS".to_string(),
                "_AUTH".to_string(),
                "_API_KEY".to_string(),
                "_PRIVATE_KEY".to_string(),
            ],
            secret_prefixes: vec![
                "SECRET_".to_string(),
                "AUTH_".to_string(),
                "CREDENTIAL_".to_string(),
            ],
            secret_exact: vec![
                "ANTHROPIC_API_KEY".to_string(),
                "OPENAI_API_KEY".to_string(),
                "AWS_SECRET_ACCESS_KEY".to_string(),
                "AWS_SESSION_TOKEN".to_string(),
                "GITHUB_TOKEN".to_string(),
                "GOOGLE_API_KEY".to_string(),
                "AZURE_CLIENT_SECRET".to_string(),
                "DATABASE_URL".to_string(),
                "REDIS_URL".to_string(),
                "PRIVATE_KEY".to_string(),
                "NPM_TOKEN".to_string(),
                "DOCKER_PASSWORD".to_string(),
                "SLACK_TOKEN".to_string(),
                "STRIPE_SECRET_KEY".to_string(),
                "TWILIO_AUTH_TOKEN".to_string(),
                "SENDGRID_API_KEY".to_string(),
            ],
            disclosure_allowlist: Vec::new(),
            redaction_placeholder: "[REDACTED]".to_string(),
        }
    }
}

impl SecretBrokerPolicy {
    /// Returns `true` if the given env var name matches a known secret pattern.
    #[must_use]
    pub fn is_secret(&self, name: &str) -> bool {
        if !self.enabled {
            return false;
        }

        let upper = name.to_ascii_uppercase();

        // Check disclosure allowlist first (overrides everything).
        if self
            .disclosure_allowlist
            .iter()
            .any(|a| a.eq_ignore_ascii_case(name))
        {
            return false;
        }

        // Exact match.
        if self
            .secret_exact
            .iter()
            .any(|e| e.eq_ignore_ascii_case(name))
        {
            return true;
        }

        // Suffix match.
        if self
            .secret_suffixes
            .iter()
            .any(|s| upper.ends_with(&s.to_ascii_uppercase()))
        {
            return true;
        }

        // Prefix match.
        self.secret_prefixes
            .iter()
            .any(|p| upper.starts_with(&p.to_ascii_uppercase()))
    }

    /// Redact a value if the env var name is a secret.
    ///
    /// Returns the original value if not a secret, or the redaction
    /// placeholder if it is.
    #[must_use]
    pub fn maybe_redact<'a>(&'a self, name: &str, value: &'a str) -> &'a str {
        if self.is_secret(name) {
            &self.redaction_placeholder
        } else {
            value
        }
    }
}

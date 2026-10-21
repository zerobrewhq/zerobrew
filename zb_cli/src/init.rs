use console::style;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::ui::{PromptDefault, StdUi};
use zb_io::validate_privileged_path;

#[derive(Debug)]
pub enum InitError {
    Message(String),
}

impl std::fmt::Display for InitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InitError::Message(msg) => f.write_str(msg),
        }
    }
}

impl From<std::io::Error> for InitError {
    fn from(err: std::io::Error) -> Self {
        InitError::Message(format!("I/O error: {err}"))
    }
}

pub fn needs_init(root: &Path, prefix: &Path) -> bool {
    let root_ok = root.exists() && is_writable(root);
    let prefix_ok = prefix.exists() && is_writable(prefix);
    !(root_ok && prefix_ok)
}

pub fn is_writable(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    let test_file = path.join(".zb_write_test");
    match std::fs::write(&test_file, b"test") {
        Ok(_) => {
            let _ = std::fs::remove_file(&test_file);
            true
        }
        Err(_) => false,
    }
}

/// Longest Homebrew prefix we may need to replace in Mach-O binaries.
/// On macOS, paths inside Mach-O headers are fixed-size, so the replacement
/// prefix must be no longer than the original.  `/opt/homebrew` = 13 chars.
const MAX_PREFIX_LEN_MACOS: usize = 13;

pub fn run_init(
    root: &Path,
    prefix: &Path,
    no_modify_path: bool,
    ui: &mut StdUi,
) -> Result<(), InitError> {
    validate_privileged_path(root)
        .map_err(|e| InitError::Message(format!("invalid root path: {e}")))?;
    validate_privileged_path(prefix)
        .map_err(|e| InitError::Message(format!("invalid prefix path: {e}")))?;

    // On macOS, warn early if the chosen prefix is too long for Mach-O patching.
    if cfg!(target_os = "macos") {
        let prefix_str = prefix.to_string_lossy();
        if prefix_str.len() > MAX_PREFIX_LEN_MACOS {
            ui.note(format!(
                "Prefix \"{}\" ({} chars) exceeds the macOS Mach-O limit of {} characters.",
                prefix_str,
                prefix_str.len(),
                MAX_PREFIX_LEN_MACOS,
            ))?;
            ui.info("Path-sensitive packages (e.g. git, curl) will fail to install.")?;
            ui.info(format!(
                "Consider a shorter prefix, e.g.: {}",
                style(format!(
                    "zb --root {} --prefix /opt/zerobrew init",
                    root.display()
                ))
                .cyan(),
            ))?;
            ui.info("If this came from old shell config, unset ZEROBREW_PREFIX and rerun init.")?;
            ui.blank_line()?;
        }
    }

    ui.heading("Initializing zerobrew...")?;

    let zerobrew_dir = match std::env::var("ZEROBREW_DIR") {
        Ok(dir) => dir,
        Err(_) => {
            let home = std::env::var("HOME")
                .map_err(|_| InitError::Message("HOME not set".to_string()))?;
            format!("{}/.zerobrew", home)
        }
    };
    let zerobrew_bin = std::env::var("ZEROBREW_BIN")
        .ok()
        .filter(|bin| !bin.is_empty())
        .unwrap_or_else(|| format!("{}/bin", zerobrew_dir));

    let dirs_to_create: Vec<PathBuf> = vec![
        root.to_path_buf(),
        root.join("store"),
        root.join("db"),
        root.join("cache"),
        root.join("locks"),
        prefix.to_path_buf(),
        prefix.join("bin"),
        prefix.join("Cellar"),
    ];

    let need_sudo = dirs_to_create.iter().any(|d| {
        if d.exists() {
            !is_writable(d)
        } else {
            d.parent()
                .map(|p| p.exists() && !is_writable(p))
                .unwrap_or(true)
        }
    });

    if need_sudo {
        ui.info("Creating directories (requires sudo)...")?;

        for dir in &dirs_to_create {
            let status = Command::new("sudo")
                .args(["mkdir", "-p", &dir.to_string_lossy()])
                .status()
                .map_err(|e| InitError::Message(format!("Failed to run sudo mkdir: {}", e)))?;

            if !status.success() {
                return Err(InitError::Message(format!(
                    "Failed to create directory: {}",
                    dir.display()
                )));
            }
        }

        let user = Command::new("whoami")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .or_else(|| std::env::var("USER").ok())
            .ok_or_else(|| {
                InitError::Message(
                    "Could not determine current user (whoami failed and $USER not set)".into(),
                )
            })?;

        let status = Command::new("sudo")
            .args(["chown", "-R", &user, &root.to_string_lossy()])
            .status()
            .map_err(|e| InitError::Message(format!("Failed to run sudo chown: {}", e)))?;

        if !status.success() {
            return Err(InitError::Message(format!(
                "Failed to set ownership on {}",
                root.display()
            )));
        }

        let status = Command::new("sudo")
            .args(["chown", "-R", &user, &prefix.to_string_lossy()])
            .status()
            .map_err(|e| InitError::Message(format!("Failed to run sudo chown: {}", e)))?;

        if !status.success() {
            return Err(InitError::Message(format!(
                "Failed to set ownership on {}",
                prefix.display()
            )));
        }
    } else {
        for dir in &dirs_to_create {
            std::fs::create_dir_all(dir).map_err(|e| {
                InitError::Message(format!("Failed to create {}: {}", dir.display(), e))
            })?;
        }
    }

    add_to_path(
        prefix,
        &zerobrew_dir,
        &zerobrew_bin,
        root,
        no_modify_path,
        ui,
    )?;

    ui.heading("Initialization complete!")?;

    Ok(())
}

const ZB_BLOCK_START: &str = "# >>> zerobrew >>>";
const ZB_BLOCK_END: &str = "# <<< zerobrew <<<";

fn upsert_managed_block(existing: &str, managed_block: &str) -> String {
    if let Some(start_idx) = existing.find(ZB_BLOCK_START)
        && let Some(end_rel_idx) = existing[start_idx..].find(ZB_BLOCK_END)
    {
        let mut end_idx = start_idx + end_rel_idx + ZB_BLOCK_END.len();
        if existing[end_idx..].starts_with("\r\n") {
            end_idx += 2;
        } else if existing[end_idx..].starts_with('\n') {
            end_idx += 1;
        }
        let mut out = String::with_capacity(existing.len() + managed_block.len());
        out.push_str(&existing[..start_idx]);
        out.push_str(managed_block);
        out.push_str(&existing[end_idx..]);
        return out;
    }

    if existing.trim().is_empty() {
        managed_block.to_string()
    } else {
        let mut out = String::with_capacity(existing.len() + managed_block.len() + 1);
        out.push_str(existing);
        if !existing.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(managed_block);
        out
    }
}

fn posix_shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }

    format!("'{}'", value.replace('\'', "'\\''"))
}

fn fish_shell_quote(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('$', "\\$")
    )
}

fn add_to_path(
    prefix: &Path,
    zerobrew_dir: &str,
    zerobrew_bin: &str,
    root: &Path,
    no_modify_path: bool,
    ui: &mut StdUi,
) -> Result<(), InitError> {
    #[derive(Clone, Copy)]
    enum ShellConfigKind {
        Posix,
        Fish,
    }

    let shell = std::env::var("SHELL").unwrap_or_default();
    let home = std::env::var("HOME").map_err(|_| InitError::Message("HOME not set".to_string()))?;

    let (config_file, shell_kind) = if shell.contains("zsh") {
        let zdotdir = std::env::var("ZDOTDIR").unwrap_or_else(|_| home.clone());
        let zshenv = format!("{}/.zshenv", zdotdir);
        let zshrc = format!("{}/.zshrc", zdotdir);
        let home_zshrc = format!("{}/.zshrc", home);

        if std::path::Path::new(&zshenv).exists() {
            (zshenv, ShellConfigKind::Posix)
        } else if std::path::Path::new(&zshrc).exists() {
            (zshrc, ShellConfigKind::Posix)
        } else {
            (home_zshrc, ShellConfigKind::Posix)
        }
    } else if shell.contains("bash") {
        let bash_profile = format!("{}/.bash_profile", home);
        if std::path::Path::new(&bash_profile).exists() {
            (bash_profile, ShellConfigKind::Posix)
        } else {
            (format!("{}/.bashrc", home), ShellConfigKind::Posix)
        }
    } else if shell.contains("fish") {
        (
            format!("{}/.config/fish/conf.d/zerobrew.fish", home),
            ShellConfigKind::Fish,
        )
    } else {
        (format!("{}/.profile", home), ShellConfigKind::Posix)
    };

    let prefix_bin = prefix.join("bin");
    let prefix_sbin = prefix.join("sbin");
    let root_str = root.display().to_string();
    let prefix_str = prefix.display().to_string();
    let prefix_bin_str = prefix_bin.display().to_string();
    let prefix_sbin_str = prefix_sbin.display().to_string();
    // Treat only a missing file as an empty config. Any other read error
    // (for example non-UTF-8 content) must not read as empty: the write
    // below truncates, and that would replace the user's shell config
    // with only the managed block.
    let existing_config = match std::fs::read_to_string(&config_file) {
        Ok(contents) => Ok(contents),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e),
    };

    if !no_modify_path {
        let block_body = match shell_kind {
            ShellConfigKind::Posix => format!(
                r#"
# zerobrew
export ZEROBREW_DIR={zerobrew_dir}
export ZEROBREW_BIN={zerobrew_bin}
export ZEROBREW_ROOT={root}
export ZEROBREW_PREFIX={prefix}
export PKG_CONFIG_PATH="$ZEROBREW_PREFIX/lib/pkgconfig:${{PKG_CONFIG_PATH:-}}"

# SSL/TLS certificates (only if ca-certificates is installed)
if [ -z "${{CURL_CA_BUNDLE:-}}" ] || [ -z "${{SSL_CERT_FILE:-}}" ]; then
  if [ -f "$ZEROBREW_PREFIX/opt/ca-certificates/share/ca-certificates/cacert.pem" ]; then
    [ -z "${{CURL_CA_BUNDLE:-}}" ] && export CURL_CA_BUNDLE="$ZEROBREW_PREFIX/opt/ca-certificates/share/ca-certificates/cacert.pem"
    [ -z "${{SSL_CERT_FILE:-}}" ] && export SSL_CERT_FILE="$ZEROBREW_PREFIX/opt/ca-certificates/share/ca-certificates/cacert.pem"
  elif [ -f "$ZEROBREW_PREFIX/etc/ca-certificates/cacert.pem" ]; then
    [ -z "${{CURL_CA_BUNDLE:-}}" ] && export CURL_CA_BUNDLE="$ZEROBREW_PREFIX/etc/ca-certificates/cacert.pem"
    [ -z "${{SSL_CERT_FILE:-}}" ] && export SSL_CERT_FILE="$ZEROBREW_PREFIX/etc/ca-certificates/cacert.pem"
  elif [ -f "$ZEROBREW_PREFIX/etc/openssl/cert.pem" ]; then
    [ -z "${{CURL_CA_BUNDLE:-}}" ] && export CURL_CA_BUNDLE="$ZEROBREW_PREFIX/etc/openssl/cert.pem"
    [ -z "${{SSL_CERT_FILE:-}}" ] && export SSL_CERT_FILE="$ZEROBREW_PREFIX/etc/openssl/cert.pem"
  elif [ -f "$ZEROBREW_PREFIX/share/ca-certificates/cacert.pem" ]; then
    [ -z "${{CURL_CA_BUNDLE:-}}" ] && export CURL_CA_BUNDLE="$ZEROBREW_PREFIX/share/ca-certificates/cacert.pem"
    [ -z "${{SSL_CERT_FILE:-}}" ] && export SSL_CERT_FILE="$ZEROBREW_PREFIX/share/ca-certificates/cacert.pem"
  fi
fi

if [ -z "${{SSL_CERT_DIR:-}}" ]; then
  if [ -d "$ZEROBREW_PREFIX/etc/ca-certificates" ]; then
    export SSL_CERT_DIR="$ZEROBREW_PREFIX/etc/ca-certificates"
  elif [ -d "$ZEROBREW_PREFIX/etc/openssl/certs" ]; then
    export SSL_CERT_DIR="$ZEROBREW_PREFIX/etc/openssl/certs"
  elif [ -d "$ZEROBREW_PREFIX/share/ca-certificates" ]; then
    export SSL_CERT_DIR="$ZEROBREW_PREFIX/share/ca-certificates"
  fi
fi

# Helper function to safely append to PATH
_zb_path_append() {{
    local argpath="$1"
    case ":${{PATH}}:" in
        *:"$argpath":*) ;;
        *) export PATH="$argpath:$PATH" ;;
    esac;
}}

_zb_path_append "$ZEROBREW_BIN"
_zb_path_append "$ZEROBREW_PREFIX/sbin"
_zb_path_append "$ZEROBREW_PREFIX/bin"
"#,
                zerobrew_dir = posix_shell_quote(zerobrew_dir),
                zerobrew_bin = posix_shell_quote(zerobrew_bin),
                root = posix_shell_quote(&root_str),
                prefix = posix_shell_quote(&prefix_str)
            ),
            ShellConfigKind::Fish => format!(
                r#"
# zerobrew
set -gx ZEROBREW_DIR {zerobrew_dir}
set -gx ZEROBREW_BIN {zerobrew_bin}
set -gx ZEROBREW_ROOT {root}
set -gx ZEROBREW_PREFIX {prefix}
if set -q PKG_CONFIG_PATH
    set -gx PKG_CONFIG_PATH "$ZEROBREW_PREFIX/lib/pkgconfig" $PKG_CONFIG_PATH
else
    set -gx PKG_CONFIG_PATH "$ZEROBREW_PREFIX/lib/pkgconfig"
end

# SSL/TLS certificates (only if ca-certificates is installed)
if not set -q CURL_CA_BUNDLE; or not set -q SSL_CERT_FILE
    if test -f "$ZEROBREW_PREFIX/opt/ca-certificates/share/ca-certificates/cacert.pem"
        set -q CURL_CA_BUNDLE; or set -gx CURL_CA_BUNDLE "$ZEROBREW_PREFIX/opt/ca-certificates/share/ca-certificates/cacert.pem"
        set -q SSL_CERT_FILE; or set -gx SSL_CERT_FILE "$ZEROBREW_PREFIX/opt/ca-certificates/share/ca-certificates/cacert.pem"
    else if test -f "$ZEROBREW_PREFIX/etc/ca-certificates/cacert.pem"
        set -q CURL_CA_BUNDLE; or set -gx CURL_CA_BUNDLE "$ZEROBREW_PREFIX/etc/ca-certificates/cacert.pem"
        set -q SSL_CERT_FILE; or set -gx SSL_CERT_FILE "$ZEROBREW_PREFIX/etc/ca-certificates/cacert.pem"
    else if test -f "$ZEROBREW_PREFIX/etc/openssl/cert.pem"
        set -q CURL_CA_BUNDLE; or set -gx CURL_CA_BUNDLE "$ZEROBREW_PREFIX/etc/openssl/cert.pem"
        set -q SSL_CERT_FILE; or set -gx SSL_CERT_FILE "$ZEROBREW_PREFIX/etc/openssl/cert.pem"
    else if test -f "$ZEROBREW_PREFIX/share/ca-certificates/cacert.pem"
        set -q CURL_CA_BUNDLE; or set -gx CURL_CA_BUNDLE "$ZEROBREW_PREFIX/share/ca-certificates/cacert.pem"
        set -q SSL_CERT_FILE; or set -gx SSL_CERT_FILE "$ZEROBREW_PREFIX/share/ca-certificates/cacert.pem"
    end
end

if not set -q SSL_CERT_DIR
    if test -d "$ZEROBREW_PREFIX/etc/ca-certificates"
        set -gx SSL_CERT_DIR "$ZEROBREW_PREFIX/etc/ca-certificates"
    else if test -d "$ZEROBREW_PREFIX/etc/openssl/certs"
        set -gx SSL_CERT_DIR "$ZEROBREW_PREFIX/etc/openssl/certs"
    else if test -d "$ZEROBREW_PREFIX/share/ca-certificates"
        set -gx SSL_CERT_DIR "$ZEROBREW_PREFIX/share/ca-certificates"
    end
end

if not contains -- "$ZEROBREW_BIN" $PATH
    set -gx PATH "$ZEROBREW_BIN" $PATH
end
if not contains -- "$ZEROBREW_PREFIX/sbin" $PATH
    set -gx PATH "$ZEROBREW_PREFIX/sbin" $PATH
end
if not contains -- "$ZEROBREW_PREFIX/bin" $PATH
    set -gx PATH "$ZEROBREW_PREFIX/bin" $PATH
end
"#,
                zerobrew_dir = fish_shell_quote(zerobrew_dir),
                zerobrew_bin = fish_shell_quote(zerobrew_bin),
                root = fish_shell_quote(&root_str),
                prefix = fish_shell_quote(&prefix_str)
            ),
        };
        let managed_block = format!("{ZB_BLOCK_START}{block_body}\n{ZB_BLOCK_END}\n");
        let existing_config = match existing_config {
            Ok(contents) => contents,
            Err(e) => {
                // Never truncate a file we could not read back.
                ui.note(format!(
                    "Could not read {} due to error: {}",
                    config_file, e
                ))?;
                ui.info(format!("Please add the following to {}:", config_file))?;
                ui.info(&managed_block)?;
                return Ok(());
            }
        };
        let updated_config = upsert_managed_block(&existing_config, &managed_block);

        if let Some(parent) = std::path::Path::new(&config_file).parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                InitError::Message(format!(
                    "Failed to create shell config directory {}: {}",
                    parent.display(),
                    e
                ))
            })?;
        }

        let write_result = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&config_file)
            .and_then(|mut f| f.write_all(updated_config.as_bytes()));

        if let Err(e) = write_result {
            ui.note(format!(
                "Could not write to {} due to error: {}",
                config_file, e
            ))?;
            ui.info(format!("Please add the following to {}:", config_file))?;
            ui.info(&managed_block)?;
        } else {
            ui.info(format!("Updated zerobrew configuration in {}", config_file))?;
            ui.info(format!(
                "Added {}, {} and {} to PATH",
                zerobrew_bin,
                prefix_bin.display(),
                prefix_sbin.display()
            ))?;
            let reload_command = match shell_kind {
                ShellConfigKind::Posix => format!(". {}", posix_shell_quote(&config_file)),
                ShellConfigKind::Fish => format!("source {}", fish_shell_quote(&config_file)),
            };
            ui.info(format!("Restart your terminal or run: {}", reload_command))?;
        }
    } else if no_modify_path {
        ui.info("Skipped shell configuration (--no-modify-path)")?;
        match shell_kind {
            ShellConfigKind::Posix => {
                ui.info("Run this in your current shell:")?;
                ui.println(format!(
                    "    export ZEROBREW_DIR={}",
                    posix_shell_quote(zerobrew_dir)
                ))?;
                ui.println(format!(
                    "    export ZEROBREW_BIN={}",
                    posix_shell_quote(zerobrew_bin)
                ))?;
                ui.println(format!(
                    "    export ZEROBREW_ROOT={}",
                    posix_shell_quote(&root_str)
                ))?;
                ui.println(format!(
                    "    export ZEROBREW_PREFIX={}",
                    posix_shell_quote(&prefix_str)
                ))?;
                ui.println(format!(
                    "    export PATH={}:{}:{}:$PATH",
                    posix_shell_quote(zerobrew_bin),
                    posix_shell_quote(&prefix_bin_str),
                    posix_shell_quote(&prefix_sbin_str)
                ))?;
            }
            ShellConfigKind::Fish => {
                ui.info("Run this in your current shell:")?;
                ui.println(format!(
                    "    set -gx ZEROBREW_DIR {}",
                    fish_shell_quote(zerobrew_dir)
                ))?;
                ui.println(format!(
                    "    set -gx ZEROBREW_BIN {}",
                    fish_shell_quote(zerobrew_bin)
                ))?;
                ui.println(format!(
                    "    set -gx ZEROBREW_ROOT {}",
                    fish_shell_quote(&root_str)
                ))?;
                ui.println(format!(
                    "    set -gx ZEROBREW_PREFIX {}",
                    fish_shell_quote(&prefix_str)
                ))?;
                ui.println(format!(
                    "    set -gx PATH {} {} {} $PATH",
                    fish_shell_quote(zerobrew_bin),
                    fish_shell_quote(&prefix_bin_str),
                    fish_shell_quote(&prefix_sbin_str)
                ))?;
            }
        }
    }

    Ok(())
}

pub fn ensure_init(
    root: &Path,
    prefix: &Path,
    auto_init: bool,
    ui: &mut StdUi,
) -> Result<(), zb_core::Error> {
    if !needs_init(root, prefix) {
        return Ok(());
    }

    // Check if both stdin and stdout are TTYs
    // If stdout is not a TTY, the user won't see the prompt, so don't prompt
    // If stdin is not a TTY, we can't read input, so don't prompt
    let is_interactive = std::io::IsTerminal::is_terminal(&std::io::stdin())
        && std::io::IsTerminal::is_terminal(&std::io::stdout());

    if is_interactive && !auto_init {
        ui.note("Zerobrew needs to be initialized first.")
            .map_err(io_to_core_error)?;
        ui.info("This will create directories at:")
            .map_err(io_to_core_error)?;
        ui.bullet(root.display()).map_err(io_to_core_error)?;
        ui.bullet(prefix.display()).map_err(io_to_core_error)?;
        ui.blank_line().map_err(io_to_core_error)?;

        if !ui
            .prompt_yes_no("Initialize now? [Y/n]", PromptDefault::Yes)
            .map_err(io_to_core_error)?
        {
            return Err(zb_core::Error::StoreCorruption {
                message: "Initialization required. Run 'zb init' first.".to_string(),
            });
        }
    }
    if !is_interactive && !auto_init {
        return Err(zb_core::Error::StoreCorruption {
            message: "Initialization required. Run 'zb init' first.".to_string(),
        });
    }
    // Implicit initialization may use a temporary root/prefix. Persist shell
    // configuration only when the user explicitly runs `zb init`.
    run_init(root, prefix, true, ui).map_err(|e| match e {
        InitError::Message(msg) => zb_core::Error::StoreCorruption { message: msg },
    })
}

fn io_to_core_error(err: std::io::Error) -> zb_core::Error {
    zb_core::Error::StoreCorruption {
        message: format!("failed to write CLI output: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::Ui;
    use std::fs;
    use std::path::Path;

    fn add_to_path(
        prefix: &Path,
        zerobrew_dir: &str,
        zerobrew_bin: &str,
        root: &Path,
        no_modify_path: bool,
    ) -> Result<(), InitError> {
        let mut ui = Ui::new();
        super::add_to_path(
            prefix,
            zerobrew_dir,
            zerobrew_bin,
            root,
            no_modify_path,
            &mut ui,
        )
    }
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Mutex, OnceLock};
    use tempfile::TempDir;

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    #[test]
    fn posix_shell_quote_preserves_spaces_and_single_quotes() {
        assert_eq!(
            posix_shell_quote("/tmp/zero brew/user's dir"),
            "'/tmp/zero brew/user'\\''s dir'"
        );
    }

    #[test]
    fn fish_shell_quote_escapes_expansions() {
        assert_eq!(
            fish_shell_quote(r#"/tmp/zero brew/$root/"bin""#),
            r#""/tmp/zero brew/\$root/\"bin\"""#
        );
    }

    #[test]
    fn needs_init_when_directories_missing() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("nonexistent_root");
        let prefix = tmp.path().join("nonexistent_prefix");

        assert!(needs_init(&root, &prefix));
    }

    #[test]
    fn needs_init_when_not_writable() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        let prefix = tmp.path().join("prefix");

        fs::create_dir(&root).unwrap();
        fs::create_dir(&prefix).unwrap();

        // Make directories read-only
        let mut root_perms = fs::metadata(&root).unwrap().permissions();
        root_perms.set_mode(0o555);
        fs::set_permissions(&root, root_perms).unwrap();

        let result = needs_init(&root, &prefix);

        // Restore permissions for cleanup
        let mut root_perms = fs::metadata(&root).unwrap().permissions();
        root_perms.set_mode(0o755);
        fs::set_permissions(&root, root_perms).unwrap();

        assert!(result);
    }

    #[test]
    fn no_init_needed_when_writable() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        let prefix = tmp.path().join("prefix");

        fs::create_dir(&root).unwrap();
        fs::create_dir(&prefix).unwrap();

        assert!(!needs_init(&root, &prefix));
    }

    #[test]
    fn is_writable_returns_true_for_writable_dir() {
        let tmp = TempDir::new().unwrap();
        assert!(is_writable(tmp.path()));
    }

    #[test]
    fn is_writable_returns_false_for_nonexistent_path() {
        let tmp = TempDir::new().unwrap();
        let nonexistent = tmp.path().join("does_not_exist");
        assert!(!is_writable(&nonexistent));
    }

    #[test]
    fn is_writable_returns_false_for_readonly_dir() {
        let tmp = TempDir::new().unwrap();
        let readonly = tmp.path().join("readonly");
        fs::create_dir(&readonly).unwrap();

        let mut perms = fs::metadata(&readonly).unwrap().permissions();
        perms.set_mode(0o555);
        fs::set_permissions(&readonly, perms).unwrap();

        assert!(!is_writable(&readonly));

        // Restore permissions for cleanup
        let mut perms = fs::metadata(&readonly).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&readonly, perms).unwrap();
    }

    #[test]
    fn add_to_path_writes_core_env_vars_with_guarded_ca_setup() {
        let _lock = env_lock();
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let prefix = tmp.path().join("prefix");
        let root = tmp.path().join("root");
        let shell_config = home.join(".bashrc");
        let zerobrew_dir = "/home/user/.zerobrew";
        let zerobrew_bin = "/home/user/.zerobrew/bin";

        fs::create_dir(&prefix).unwrap();
        fs::create_dir(&root).unwrap();

        // Set up environment to simulate bash
        unsafe {
            std::env::set_var("HOME", home.to_str().unwrap());
        }
        unsafe {
            std::env::set_var("SHELL", "/bin/bash");
        }

        add_to_path(&prefix, zerobrew_dir, zerobrew_bin, &root, false).unwrap();

        let content = fs::read_to_string(&shell_config).unwrap();
        assert!(content.contains(ZB_BLOCK_START));
        assert!(content.contains(ZB_BLOCK_END));
        assert!(content.contains("export ZEROBREW_DIR='/home/user/.zerobrew'"));
        assert!(content.contains("export ZEROBREW_BIN='/home/user/.zerobrew/bin'"));
        assert!(content.contains(&format!("export ZEROBREW_ROOT='{}'", root.display())));
        assert!(content.contains(&format!("export ZEROBREW_PREFIX='{}'", prefix.display())));
        assert!(content.contains("export PKG_CONFIG_PATH="));
        assert!(content.contains("/lib/pkgconfig"));
        assert!(
            content.contains(
                "if [ -z \"${CURL_CA_BUNDLE:-}\" ] || [ -z \"${SSL_CERT_FILE:-}\" ]; then"
            )
        );
        assert!(content.contains("if [ -z \"${SSL_CERT_DIR:-}\" ]; then"));
        assert!(content.contains("CURL_CA_BUNDLE"));
        assert!(content.contains("SSL_CERT_FILE"));
        assert!(content.contains("SSL_CERT_DIR"));
        assert!(content.contains("$ZEROBREW_PREFIX/etc/openssl/cert.pem"));
        assert!(content.contains("$ZEROBREW_PREFIX/etc/openssl/certs"));
    }

    #[test]
    fn add_to_path_includes_path_append_function() {
        let _lock = env_lock();
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let prefix = tmp.path().join("prefix");
        let root = tmp.path().join("root");
        let shell_config = home.join(".bashrc");
        let zerobrew_dir = "/home/user/.zerobrew";
        let zerobrew_bin = "/home/user/.zerobrew/bin";

        fs::create_dir(&prefix).unwrap();
        fs::create_dir(&root).unwrap();

        unsafe {
            std::env::set_var("HOME", home.to_str().unwrap());
        }
        unsafe {
            std::env::set_var("SHELL", "/bin/bash");
        }

        add_to_path(&prefix, zerobrew_dir, zerobrew_bin, &root, false).unwrap();

        let content = fs::read_to_string(&shell_config).unwrap();
        assert!(content.contains("_zb_path_append()"));
        assert!(content.contains("case \":${PATH}:"));
        assert!(content.contains("_zb_path_append"));
    }

    #[test]
    fn add_to_path_adds_both_paths() {
        let _lock = env_lock();
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let prefix = tmp.path().join("prefix");
        let root = tmp.path().join("root");
        let shell_config = home.join(".bashrc");
        let zerobrew_dir = "/home/user/.zerobrew";
        let zerobrew_bin = "/home/user/.zerobrew/bin";

        fs::create_dir(&prefix).unwrap();
        fs::create_dir(&root).unwrap();

        unsafe {
            std::env::set_var("HOME", home.to_str().unwrap());
        }
        unsafe {
            std::env::set_var("SHELL", "/bin/bash");
        }

        add_to_path(&prefix, zerobrew_dir, zerobrew_bin, &root, false).unwrap();

        let content = fs::read_to_string(&shell_config).unwrap();
        assert!(content.contains("_zb_path_append \"$ZEROBREW_BIN\""));
        assert!(content.contains("_zb_path_append \"$ZEROBREW_PREFIX/bin\""));
        assert!(content.contains("_zb_path_append \"$ZEROBREW_PREFIX/sbin\""));
    }

    #[test]
    fn add_to_path_no_modify_shell_skips_write() {
        let _lock = env_lock();
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let prefix = tmp.path().join("prefix");
        let root = tmp.path().join("root");
        let shell_config = home.join(".bashrc");
        let zerobrew_dir = "/home/user/.zerobrew";
        let zerobrew_bin = "/home/user/.zerobrew/bin";

        fs::create_dir(&prefix).unwrap();
        fs::create_dir(&root).unwrap();

        unsafe {
            std::env::set_var("HOME", home.to_str().unwrap());
        }
        unsafe {
            std::env::set_var("SHELL", "/bin/bash");
        }

        add_to_path(&prefix, zerobrew_dir, zerobrew_bin, &root, true).unwrap();

        // File should not be created
        assert!(!shell_config.exists());
    }

    #[test]
    fn add_to_path_no_duplicate_config() {
        let _lock = env_lock();
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let prefix = tmp.path().join("prefix");
        let root = tmp.path().join("root");
        let shell_config = home.join(".bashrc");
        let zerobrew_dir = "/home/user/.zerobrew";
        let zerobrew_bin = "/home/user/.zerobrew/bin";

        fs::create_dir(&prefix).unwrap();
        fs::create_dir(&root).unwrap();

        unsafe {
            std::env::set_var("HOME", home.to_str().unwrap());
        }
        unsafe {
            std::env::set_var("SHELL", "/bin/bash");
        }

        // Write initial config with existing managed block and unrelated user content
        fs::write(
            &shell_config,
            format!(
                "export KEEP_ME=true\n{ZB_BLOCK_START}\n# zerobrew\nexport ZEROBREW_DIR=/old\n{ZB_BLOCK_END}\n"
            ),
        )
        .unwrap();

        add_to_path(&prefix, zerobrew_dir, zerobrew_bin, &root, false).unwrap();

        // Managed block should be replaced, preserving unrelated user content
        let content = fs::read_to_string(&shell_config).unwrap();
        assert!(content.contains("export KEEP_ME=true"));
        assert!(content.contains("export ZEROBREW_DIR='/home/user/.zerobrew'"));
        assert!(!content.contains("export ZEROBREW_DIR=/old"));
        assert_eq!(content.matches(ZB_BLOCK_START).count(), 1);
        assert_eq!(content.matches(ZB_BLOCK_END).count(), 1);
    }

    #[test]
    fn add_to_path_preserves_config_it_cannot_read() {
        let _lock = env_lock();
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let prefix = tmp.path().join("prefix");
        let root = tmp.path().join("root");
        let shell_config = home.join(".bashrc");
        let zerobrew_dir = "/home/user/.zerobrew";
        let zerobrew_bin = "/home/user/.zerobrew/bin";

        fs::create_dir(&prefix).unwrap();
        fs::create_dir(&root).unwrap();

        // Non-UTF-8 content: read_to_string fails with InvalidData.
        let original: &[u8] = b"# caf\xe9 config\nexport KEEP_ME=true\n";
        fs::write(&shell_config, original).unwrap();

        unsafe {
            std::env::set_var("HOME", home.to_str().unwrap());
            std::env::set_var("SHELL", "/bin/bash");
        }

        add_to_path(&prefix, zerobrew_dir, zerobrew_bin, &root, false).unwrap();

        // A config that cannot be read back must stay untouched.
        let after = fs::read(&shell_config).unwrap();
        assert_eq!(after, original);
    }

    #[test]
    fn add_to_path_uses_zshrc_for_zsh() {
        let _lock = env_lock();
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let prefix = tmp.path().join("prefix");
        let root = tmp.path().join("root");
        let shell_config = home.join(".zshrc");
        let zerobrew_dir = "/home/user/.zerobrew";
        let zerobrew_bin = "/home/user/.zerobrew/bin";

        fs::create_dir(&prefix).unwrap();
        fs::create_dir(&root).unwrap();

        unsafe {
            std::env::set_var("HOME", home.to_str().unwrap());
            std::env::set_var("SHELL", "/bin/zsh");
            std::env::remove_var("ZDOTDIR");
        }

        add_to_path(&prefix, zerobrew_dir, zerobrew_bin, &root, false).unwrap();

        assert!(shell_config.exists());
        let content = fs::read_to_string(&shell_config).unwrap();
        assert!(content.contains("# zerobrew"));
    }

    #[test]
    fn add_to_path_prefers_zshenv_when_exists() {
        let _lock = env_lock();
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let prefix = tmp.path().join("prefix");
        let root = tmp.path().join("root");
        let zshenv = home.join(".zshenv");
        let zshrc = home.join(".zshrc");
        let zerobrew_dir = "/home/user/.zerobrew";
        let zerobrew_bin = "/home/user/.zerobrew/bin";

        fs::create_dir(&prefix).unwrap();
        fs::create_dir(&root).unwrap();

        // Create .zshenv first
        fs::write(&zshenv, "# existing zshenv\n").unwrap();

        unsafe {
            std::env::set_var("HOME", home.to_str().unwrap());
        }
        unsafe {
            std::env::set_var("SHELL", "/bin/zsh");
        }
        unsafe {
            std::env::remove_var("ZDOTDIR");
        }

        add_to_path(&prefix, zerobrew_dir, zerobrew_bin, &root, false).unwrap();

        // Should write to .zshenv, not .zshrc
        assert!(zshenv.exists());
        let zshenv_content = fs::read_to_string(&zshenv).unwrap();
        assert!(zshenv_content.contains("# zerobrew"));
        assert!(!zshrc.exists());
    }

    #[test]
    fn add_to_path_prefers_bash_profile_when_exists() {
        let _lock = env_lock();
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let prefix = tmp.path().join("prefix");
        let root = tmp.path().join("root");
        let bash_profile = home.join(".bash_profile");
        let bashrc = home.join(".bashrc");
        let zerobrew_dir = "/home/user/.zerobrew";
        let zerobrew_bin = "/home/user/.zerobrew/bin";

        fs::create_dir(&prefix).unwrap();
        fs::create_dir(&root).unwrap();

        // Create .bash_profile first
        fs::write(&bash_profile, "# existing bash_profile\n").unwrap();

        unsafe {
            std::env::set_var("HOME", home.to_str().unwrap());
        }
        unsafe {
            std::env::set_var("SHELL", "/bin/bash");
        }

        add_to_path(&prefix, zerobrew_dir, zerobrew_bin, &root, false).unwrap();

        assert!(bash_profile.exists());
        let profile_content = fs::read_to_string(&bash_profile).unwrap();
        assert!(profile_content.contains("# zerobrew"));
        assert!(!bashrc.exists());
    }

    #[test]
    fn add_to_path_uses_profile_for_other_shells() {
        let _lock = env_lock();
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let prefix = tmp.path().join("prefix");
        let root = tmp.path().join("root");
        let profile = home.join(".profile");
        let zerobrew_dir = "/home/user/.zerobrew";
        let zerobrew_bin = "/home/user/.zerobrew/bin";

        fs::create_dir(&prefix).unwrap();
        fs::create_dir(&root).unwrap();

        unsafe {
            std::env::set_var("HOME", home.to_str().unwrap());
        }
        unsafe {
            std::env::set_var("SHELL", "/bin/sh");
        }

        add_to_path(&prefix, zerobrew_dir, zerobrew_bin, &root, false).unwrap();

        assert!(profile.exists());
        let content = fs::read_to_string(&profile).unwrap();
        assert!(content.contains("# zerobrew"));
    }

    #[test]
    fn add_to_path_uses_zdotdir_when_set() {
        let _lock = env_lock();
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let zdotdir = tmp.path().join("zsh_config");
        let prefix = tmp.path().join("prefix");
        let root = tmp.path().join("root");
        let shell_config = zdotdir.join(".zshrc");
        let zerobrew_dir = "/home/user/.zerobrew";
        let zerobrew_bin = "/home/user/.zerobrew/bin";

        fs::create_dir(&zdotdir).unwrap();
        fs::create_dir(&prefix).unwrap();
        fs::create_dir(&root).unwrap();
        fs::write(&shell_config, "# existing zshrc\n").unwrap();

        unsafe {
            std::env::set_var("HOME", home.to_str().unwrap());
        }
        unsafe {
            std::env::set_var("SHELL", "/bin/zsh");
        }
        unsafe {
            std::env::set_var("ZDOTDIR", zdotdir.to_str().unwrap());
        }

        add_to_path(&prefix, zerobrew_dir, zerobrew_bin, &root, false).unwrap();

        // Should write to $ZDOTDIR/.zshrc when it exists
        assert!(shell_config.exists());
        let content = fs::read_to_string(&shell_config).unwrap();
        assert!(content.contains("# zerobrew"));
    }

    #[test]
    fn add_to_path_uses_fish_conf_d_for_fish() {
        let _lock = env_lock();
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let prefix = tmp.path().join("prefix");
        let root = tmp.path().join("root");
        let fish_config = home.join(".config/fish/conf.d/zerobrew.fish");
        let zerobrew_dir = "/home/user/.zerobrew";
        let zerobrew_bin = "/home/user/.zerobrew/bin";

        fs::create_dir(&prefix).unwrap();
        fs::create_dir(&root).unwrap();

        unsafe {
            std::env::set_var("HOME", home.to_str().unwrap());
            std::env::set_var("SHELL", "/usr/bin/fish");
        }

        add_to_path(&prefix, zerobrew_dir, zerobrew_bin, &root, false).unwrap();

        assert!(fish_config.exists());
        let content = fs::read_to_string(&fish_config).unwrap();
        assert!(content.contains("# zerobrew"));
        assert!(content.contains("set -gx ZEROBREW_DIR"));
        assert!(content.contains("if not set -q CURL_CA_BUNDLE; or not set -q SSL_CERT_FILE"));
        assert!(content.contains("if not set -q SSL_CERT_DIR"));
        assert!(content.contains("set -q CURL_CA_BUNDLE; or set -gx CURL_CA_BUNDLE"));
        assert!(content.contains("set -q SSL_CERT_FILE; or set -gx SSL_CERT_FILE"));
        assert!(content.contains("$ZEROBREW_PREFIX/etc/openssl/cert.pem"));
        assert!(content.contains("$ZEROBREW_PREFIX/etc/openssl/certs"));
        assert!(content.contains("if set -q PKG_CONFIG_PATH"));
        assert!(content.contains(
            "set -gx PKG_CONFIG_PATH \"$ZEROBREW_PREFIX/lib/pkgconfig\" $PKG_CONFIG_PATH"
        ));
        assert!(!content.contains(
            "set -gx PKG_CONFIG_PATH \"$ZEROBREW_PREFIX/lib/pkgconfig:$PKG_CONFIG_PATH\""
        ));
        assert!(content.contains("set -gx PATH \"$ZEROBREW_PREFIX/sbin\" $PATH"));
    }

    #[test]
    fn add_to_path_falls_back_to_home_zshrc_when_zdotdir_files_missing() {
        let _lock = env_lock();
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let zdotdir = tmp.path().join("zsh_config");
        let prefix = tmp.path().join("prefix");
        let root = tmp.path().join("root");
        let zdotdir_zshrc = zdotdir.join(".zshrc");
        let home_zshrc = home.join(".zshrc");
        let zerobrew_dir = "/home/user/.zerobrew";
        let zerobrew_bin = "/home/user/.zerobrew/bin";

        fs::create_dir(&zdotdir).unwrap();
        fs::create_dir(&prefix).unwrap();
        fs::create_dir(&root).unwrap();

        unsafe {
            std::env::set_var("HOME", home.to_str().unwrap());
            std::env::set_var("SHELL", "/bin/zsh");
            std::env::set_var("ZDOTDIR", zdotdir.to_str().unwrap());
        }

        add_to_path(&prefix, zerobrew_dir, zerobrew_bin, &root, false).unwrap();

        assert!(!zdotdir_zshrc.exists());
        assert!(home_zshrc.exists());
        let content = fs::read_to_string(&home_zshrc).unwrap();
        assert!(content.contains("# zerobrew"));
    }

    #[test]
    fn upsert_managed_block_replacement_consumes_trailing_newline() {
        let managed_block =
            format!("{ZB_BLOCK_START}\n# zerobrew\nexport ZEROBREW_DIR=/new\n{ZB_BLOCK_END}\n");
        let existing = format!(
            "prefix\n{ZB_BLOCK_START}\n# zerobrew\nexport ZEROBREW_DIR=/old\n{ZB_BLOCK_END}\npostfix\n"
        );

        let first = upsert_managed_block(&existing, &managed_block);
        let second = upsert_managed_block(&first, &managed_block);

        assert_eq!(first, second);
        assert!(first.contains("# <<< zerobrew <<<\npostfix\n"));
        assert!(!first.contains("# <<< zerobrew <<<\n\npostfix\n"));
    }
}

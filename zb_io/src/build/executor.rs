use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::fs;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use zb_core::{BuildPlan, Error};

use super::environment::build_env;
use super::source::download_and_extract_source;

const SHIM_RUBY: &str = include_str!("shim.rb");

pub struct BuildExecutor {
    prefix: PathBuf,
    work_root: PathBuf,
}

impl BuildExecutor {
    pub fn new(prefix: PathBuf) -> Self {
        let work_root = prefix.join("tmp").join("build");
        Self { prefix, work_root }
    }

    pub async fn execute(
        &self,
        plan: &BuildPlan,
        formula_rb_path: &Path,
        installed_deps: &HashMap<String, DepInfo>,
    ) -> Result<(), Error> {
        let work_dir = self.work_root.join(&plan.formula_name);
        self.prepare_work_dir(&work_dir).await?;

        let source_root = download_and_extract_source(
            &plan.formula_name,
            &plan.source_url,
            plan.source_checksum.as_deref(),
            &work_dir,
            plan.nounzip,
        )
        .await?;

        let shim_path = work_dir.join("zerobrew_shim.rb");
        fs::write(&shim_path, SHIM_RUBY)
            .await
            .map_err(Error::file("failed to write ruby shim"))?;

        fs::create_dir_all(&plan.cellar_path)
            .await
            .map_err(Error::file("failed to create cellar directory"))?;

        let mut env = build_env(plan, &self.prefix);
        env.insert(
            "ZEROBREW_FORMULA_FILE".into(),
            formula_rb_path.display().to_string(),
        );

        let deps_json = serde_json::to_string(installed_deps).unwrap_or_else(|_| "{}".into());
        env.insert("ZEROBREW_INSTALLED_DEPS".into(), deps_json);

        let ruby = find_ruby().await?;
        run_build(&ruby, &shim_path, &source_root, &env).await?;

        self.cleanup_work_dir(&work_dir).await;
        Ok(())
    }

    async fn prepare_work_dir(&self, work_dir: &Path) -> Result<(), Error> {
        if work_dir.exists() {
            let _ = fs::remove_dir_all(work_dir).await;
        }
        fs::create_dir_all(work_dir)
            .await
            .map_err(Error::file("failed to create work directory"))
    }

    async fn cleanup_work_dir(&self, work_dir: &Path) {
        let _ = fs::remove_dir_all(work_dir).await;
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DepInfo {
    pub cellar_path: String,
}

async fn find_ruby() -> Result<PathBuf, Error> {
    for candidate in ["ruby", "/usr/bin/ruby"] {
        let result = Command::new(candidate).arg("--version").output().await;

        if let Ok(output) = result
            && output.status.success()
        {
            return Ok(PathBuf::from(candidate));
        }
    }

    Err(Error::ExecutionError {
        message: "ruby not found — required for building from source".into(),
    })
}

async fn run_build(
    ruby: &Path,
    shim_path: &Path,
    source_root: &Path,
    env: &HashMap<String, String>,
) -> Result<(), Error> {
    let mut child = Command::new(ruby)
        .arg(shim_path)
        .current_dir(source_root)
        .envs(env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(Error::exec("failed to execute ruby shim"))?;

    let stdout = child.stdout.take().ok_or_else(|| Error::ExecutionError {
        message: "failed to capture ruby shim stdout".to_string(),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| Error::ExecutionError {
        message: "failed to capture ruby shim stderr".to_string(),
    })?;

    let stdout_task = tokio::spawn(stream_output_and_capture_tail(stdout, false));
    let stderr_task = tokio::spawn(stream_output_and_capture_tail(stderr, true));

    let status = child
        .wait()
        .await
        .map_err(Error::exec("failed waiting for ruby shim"))?;

    let stdout_tail = stdout_task
        .await
        .map_err(Error::exec("failed to join stdout task"))?
        .map_err(Error::exec("failed reading stdout"))?;
    let stderr_tail = stderr_task
        .await
        .map_err(Error::exec("failed to join stderr task"))?
        .map_err(Error::exec("failed reading stderr"))?;

    if !status.success() {
        let mut msg = format!("source build failed (exit code: {:?})", status.code());
        let tail = if !stderr_tail.is_empty() {
            stderr_tail
        } else {
            stdout_tail
        };
        if !tail.is_empty() {
            msg.push('\n');
            msg.push_str(&tail.join("\n"));
        }
        return Err(Error::ExecutionError { message: msg });
    }

    Ok(())
}

async fn stream_output_and_capture_tail<R>(
    reader: R,
    stderr: bool,
) -> Result<Vec<String>, std::io::Error>
where
    R: AsyncRead + Unpin,
{
    const TAIL_LINES: usize = 40;
    let mut tail = VecDeque::with_capacity(TAIL_LINES);
    let mut lines = BufReader::new(reader).lines();

    while let Some(line) = lines.next_line().await? {
        if stderr {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }

        if tail.len() == TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(line);
    }

    Ok(tail.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_build_supports_mv_in_formula_install() {
        let Some(ruby) = find_ruby().await.ok() else {
            return;
        };

        let tmp = tempfile::tempdir().unwrap();
        let source_root = tmp.path().join("source");
        std::fs::create_dir_all(source_root.join("themes")).unwrap();
        std::fs::write(source_root.join("themes/default.omp.json"), "{}").unwrap();

        let shim_path = tmp.path().join("shim.rb");
        std::fs::write(&shim_path, SHIM_RUBY).unwrap();

        let formula_path = tmp.path().join("foo.rb");
        std::fs::write(
            &formula_path,
            r#"
class Foo < Formula
  def install
    mv "themes", prefix
  end
end
"#,
        )
        .unwrap();

        let prefix = tmp.path().join("prefix");
        let cellar = prefix.join("Cellar");
        std::fs::create_dir_all(&cellar).unwrap();

        let mut env = HashMap::new();
        env.insert("ZEROBREW_PREFIX".to_string(), prefix.display().to_string());
        env.insert("ZEROBREW_CELLAR".to_string(), cellar.display().to_string());
        env.insert("ZEROBREW_FORMULA_NAME".to_string(), "foo".to_string());
        env.insert("ZEROBREW_FORMULA_VERSION".to_string(), "1.0.0".to_string());
        env.insert(
            "ZEROBREW_FORMULA_FILE".to_string(),
            formula_path.display().to_string(),
        );
        env.insert("ZEROBREW_INSTALLED_DEPS".to_string(), "{}".to_string());

        run_build(&ruby, &shim_path, &source_root, &env)
            .await
            .unwrap();

        assert!(
            prefix
                .join("Cellar")
                .join("foo")
                .join("1.0.0")
                .join("themes")
                .join("default.omp.json")
                .exists()
        );
    }

    #[tokio::test]
    async fn run_build_includes_stderr_tail_in_error() {
        let Some(ruby) = find_ruby().await.ok() else {
            return;
        };

        let tmp = tempfile::tempdir().unwrap();
        let source_root = tmp.path().join("source");
        std::fs::create_dir_all(&source_root).unwrap();

        let shim_path = tmp.path().join("shim.rb");
        std::fs::write(&shim_path, SHIM_RUBY).unwrap();

        let formula_path = tmp.path().join("foo.rb");
        std::fs::write(
            &formula_path,
            r#"
class Foo < Formula
  def install
    system "sh", "-c", "echo boom-from-stderr 1>&2; exit 7"
  end
end
"#,
        )
        .unwrap();

        let prefix = tmp.path().join("prefix");
        let cellar = prefix.join("Cellar");
        std::fs::create_dir_all(&cellar).unwrap();

        let mut env = HashMap::new();
        env.insert("ZEROBREW_PREFIX".to_string(), prefix.display().to_string());
        env.insert("ZEROBREW_CELLAR".to_string(), cellar.display().to_string());
        env.insert("ZEROBREW_FORMULA_NAME".to_string(), "foo".to_string());
        env.insert("ZEROBREW_FORMULA_VERSION".to_string(), "1.0.0".to_string());
        env.insert(
            "ZEROBREW_FORMULA_FILE".to_string(),
            formula_path.display().to_string(),
        );
        env.insert("ZEROBREW_INSTALLED_DEPS".to_string(), "{}".to_string());

        let err = run_build(&ruby, &shim_path, &source_root, &env)
            .await
            .unwrap_err();

        let message = err.to_string();
        assert!(message.contains("source build failed"));
        assert!(message.contains("boom-from-stderr"));
    }

    fn sha256_hex_file(path: &Path) -> String {
        use sha2::{Digest, Sha256};
        let bytes = std::fs::read(path).unwrap();
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hasher
            .finalize()
            .iter()
            .fold(String::with_capacity(64), |mut s, b| {
                use std::fmt::Write;
                let _ = write!(s, "{b:02x}");
                s
            })
    }

    fn file_url(path: &Path) -> String {
        format!("file://{}", path.canonicalize().unwrap().display())
    }

    async fn run_shim_formula(
        ruby: &Path,
        source_root: &Path,
        prefix: &Path,
        formula_rb: &str,
    ) -> Result<(), Error> {
        let tmp = source_root.parent().unwrap();
        let shim_path = tmp.join("shim.rb");
        std::fs::write(&shim_path, SHIM_RUBY).unwrap();

        let formula_path = tmp.join("formula.rb");
        std::fs::write(&formula_path, formula_rb).unwrap();

        let cellar = prefix.join("Cellar");
        std::fs::create_dir_all(&cellar).unwrap();

        let mut env = HashMap::new();
        env.insert("ZEROBREW_PREFIX".to_string(), prefix.display().to_string());
        env.insert("ZEROBREW_CELLAR".to_string(), cellar.display().to_string());
        env.insert("ZEROBREW_FORMULA_NAME".to_string(), "foo".to_string());
        env.insert("ZEROBREW_FORMULA_VERSION".to_string(), "1.0.0".to_string());
        env.insert(
            "ZEROBREW_FORMULA_FILE".to_string(),
            formula_path.display().to_string(),
        );
        env.insert("ZEROBREW_INSTALLED_DEPS".to_string(), "{}".to_string());

        run_build(ruby, &shim_path, source_root, &env).await
    }

    #[tokio::test]
    async fn run_build_rejects_resource_checksum_mismatch() {
        let Some(ruby) = find_ruby().await.ok() else {
            return;
        };

        let tmp = tempfile::tempdir().unwrap();
        let source_root = tmp.path().join("source");
        std::fs::create_dir_all(&source_root).unwrap();

        let resource_dir = tmp.path().join("resource_content");
        std::fs::create_dir_all(&resource_dir).unwrap();
        std::fs::write(resource_dir.join("file.txt"), "resource-bytes").unwrap();

        let archive = tmp.path().join("resource.tar.gz");
        std::process::Command::new("tar")
            .args([
                "czf",
                archive.to_str().unwrap(),
                "-C",
                tmp.path().to_str().unwrap(),
                "resource_content",
            ])
            .status()
            .unwrap()
            .success()
            .then_some(())
            .expect("tar failed to create resource archive");

        let resource_url = file_url(&archive);
        let formula = format!(
            r#"
class Foo < Formula
  resource "dep" do
    url "{resource_url}"
    sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  end

  def install
    resource("dep").stage do |_r|
      File.write(prefix + "done", "ok")
    end
  end
end
"#
        );

        let prefix = tmp.path().join("prefix");
        let err = run_shim_formula(&ruby, &source_root, &prefix, &formula)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("checksum mismatch"));
    }

    #[tokio::test]
    async fn run_build_accepts_matching_resource_checksum() {
        let Some(ruby) = find_ruby().await.ok() else {
            return;
        };

        let tmp = tempfile::tempdir().unwrap();
        let source_root = tmp.path().join("source");
        std::fs::create_dir_all(&source_root).unwrap();

        let resource_dir = tmp.path().join("resource_content");
        std::fs::create_dir_all(&resource_dir).unwrap();
        std::fs::write(resource_dir.join("file.txt"), "resource-bytes").unwrap();

        let archive = tmp.path().join("resource.tar.gz");
        std::process::Command::new("tar")
            .args([
                "czf",
                archive.to_str().unwrap(),
                "-C",
                tmp.path().to_str().unwrap(),
                "resource_content",
            ])
            .status()
            .unwrap()
            .success()
            .then_some(())
            .expect("tar failed to create resource archive");

        let resource_url = file_url(&archive);
        let resource_sha = sha256_hex_file(&archive);
        let formula = format!(
            r#"
class Foo < Formula
  resource "dep" do
    url "{resource_url}"
    sha256 "{resource_sha}"
  end

  def install
    resource("dep").stage do |_r|
      File.write(prefix + "done", "ok")
    end
  end
end
"#
        );

        let prefix = tmp.path().join("prefix");
        run_shim_formula(&ruby, &source_root, &prefix, &formula)
            .await
            .unwrap();

        assert!(
            prefix
                .join("Cellar")
                .join("foo")
                .join("1.0.0")
                .join("done")
                .exists()
        );
    }

    #[tokio::test]
    async fn run_build_rejects_patch_checksum_mismatch() {
        let Some(ruby) = find_ruby().await.ok() else {
            return;
        };

        let tmp = tempfile::tempdir().unwrap();
        let source_root = tmp.path().join("source");
        std::fs::create_dir_all(&source_root).unwrap();
        std::fs::write(source_root.join("hello.txt"), "hello\n").unwrap();

        let patch = tmp.path().join("change.patch");
        std::fs::write(
            &patch,
            "--- a/hello.txt\n+++ b/hello.txt\n@@ -1 +1 @@\n-hello\n+world\n",
        )
        .unwrap();

        let patch_url = file_url(&patch);
        let formula = format!(
            r#"
class Foo < Formula
  patch :p0 do
    url "{patch_url}"
    sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  end

  def install
    File.write(prefix + "done", File.read("hello.txt"))
  end
end
"#
        );

        let prefix = tmp.path().join("prefix");
        let err = run_shim_formula(&ruby, &source_root, &prefix, &formula)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("checksum mismatch"));
    }

    #[tokio::test]
    async fn run_build_supports_class_level_build_options_and_cd() {
        let Some(ruby) = find_ruby().await.ok() else {
            return;
        };

        let tmp = tempfile::tempdir().unwrap();
        let source_root = tmp.path().join("source");
        std::fs::create_dir_all(&source_root).unwrap();
        std::fs::write(source_root.join("main.go"), "package main\n").unwrap();

        // Mirrors https://github.com/xo/homebrew-xo/blob/master/Formula/usql.rb
        let formula = r#"
$tags = %w(most)

class Foo < Formula
  option "with-odbc", "Build with ODBC support"

  if build.with? "odbc" then
    $tags << "odbc"
    depends_on "unixodbc"
  end

  $tags << "no-extra" if build.without? "extra"

  def install
    (buildpath/"src/example.com/foo").install buildpath.children
    cd "src/example.com/foo" do
      File.write(prefix + "tags", $tags.join(" "))
      raise "source not installed" unless File.exist?("main.go")
      File.write(prefix + "cwd", Dir.pwd)
    end
    File.write(prefix + "after", Dir.pwd)
  end
end
"#;

        let prefix = tmp.path().join("prefix");
        run_shim_formula(&ruby, &source_root, &prefix, formula)
            .await
            .unwrap();

        let keg = prefix.join("Cellar").join("foo").join("1.0.0");
        assert_eq!(
            std::fs::read_to_string(keg.join("tags")).unwrap(),
            "most no-extra"
        );
        let cwd = std::fs::read_to_string(keg.join("cwd")).unwrap();
        assert!(cwd.ends_with("src/example.com/foo"), "cwd: {cwd}");
        let after = std::fs::read_to_string(keg.join("after")).unwrap();
        assert!(!after.ends_with("src/example.com/foo"), "after: {after}");
    }

    #[tokio::test]
    async fn run_build_with_env_applies_symbol_key_and_pathname_then_restores() {
        let Some(ruby) = find_ruby().await.ok() else {
            return;
        };

        let tmp = tempfile::tempdir().unwrap();
        let source_root = tmp.path().join("source");
        std::fs::create_dir_all(&source_root).unwrap();

        let formula = r#"
class Foo < Formula
  def install
    before = ENV["HOME"]
    seen = with_env(HOME: buildpath) { ENV["HOME"] }
    raise "HOME not set from Pathname" unless seen == buildpath.to_s
    raise "HOME not restored" unless ENV["HOME"] == before

    positional = with_env({ "ZB_WITH_ENV_POS" => "from-hash" }) { ENV["ZB_WITH_ENV_POS"] }
    raise "positional hash not applied" unless positional == "from-hash"
    raise "positional hash not removed" unless ENV["ZB_WITH_ENV_POS"].nil?

    File.write(prefix + "ok", "ok")
  end
end
"#;

        let prefix = tmp.path().join("prefix");
        run_shim_formula(&ruby, &source_root, &prefix, formula)
            .await
            .unwrap();

        let ok = prefix.join("Cellar").join("foo").join("1.0.0").join("ok");
        assert_eq!(std::fs::read_to_string(ok).unwrap(), "ok");
    }

    #[tokio::test]
    async fn run_build_with_env_unsets_nil_and_restores_after_raise() {
        let Some(ruby) = find_ruby().await.ok() else {
            return;
        };

        let tmp = tempfile::tempdir().unwrap();
        let source_root = tmp.path().join("source");
        std::fs::create_dir_all(&source_root).unwrap();

        let formula = r#"
class Foo < Formula
  def install
    ENV["ZB_WITH_ENV_NIL"] = "original"
    ENV["ZB_WITH_ENV_FALSE"] = "original"

    begin
      with_env(ZB_WITH_ENV_NIL: nil, ZB_WITH_ENV_FALSE: false) do
        File.write(prefix + "nil-inside", ENV["ZB_WITH_ENV_NIL"].nil? ? "unset" : "set")
        File.write(prefix + "false-inside", ENV["ZB_WITH_ENV_FALSE"].to_s)
        raise "boom"
      end
    rescue RuntimeError => e
      raise "unexpected error" unless e.message == "boom"
    end

    File.write(prefix + "nil-after", ENV["ZB_WITH_ENV_NIL"].to_s)
    File.write(prefix + "false-after", ENV["ZB_WITH_ENV_FALSE"].to_s)
  end
end
"#;

        let prefix = tmp.path().join("prefix");
        run_shim_formula(&ruby, &source_root, &prefix, formula)
            .await
            .unwrap();

        let keg = prefix.join("Cellar").join("foo").join("1.0.0");
        assert_eq!(
            std::fs::read_to_string(keg.join("nil-inside")).unwrap(),
            "unset"
        );
        assert_eq!(
            std::fs::read_to_string(keg.join("false-inside")).unwrap(),
            "false"
        );
        assert_eq!(
            std::fs::read_to_string(keg.join("nil-after")).unwrap(),
            "original"
        );
        assert_eq!(
            std::fs::read_to_string(keg.join("false-after")).unwrap(),
            "original"
        );
    }

    #[tokio::test]
    async fn run_build_with_env_restores_scoped_key_and_keeps_other_env_edits() {
        let Some(ruby) = find_ruby().await.ok() else {
            return;
        };

        let tmp = tempfile::tempdir().unwrap();
        let source_root = tmp.path().join("source");
        std::fs::create_dir_all(&source_root).unwrap();

        let formula = r#"
class Foo < Formula
  def install
    ENV["ZB_SCOPED"] = "before"
    ENV["ZB_OTHER"] = "keep"

    with_env(ZB_SCOPED: "temp") do
      ENV.append("ZB_SCOPED", "more")
      ENV.append_path("ZB_OTHER", "/opt/zb-extra")
      File.write(prefix + "scoped-inside", ENV["ZB_SCOPED"])
      File.write(prefix + "other-inside", ENV["ZB_OTHER"])
    end

    File.write(prefix + "scoped-after", ENV["ZB_SCOPED"])
    File.write(prefix + "other-after", ENV["ZB_OTHER"])
  end
end
"#;

        let prefix = tmp.path().join("prefix");
        run_shim_formula(&ruby, &source_root, &prefix, formula)
            .await
            .unwrap();

        let keg = prefix.join("Cellar").join("foo").join("1.0.0");
        // The scoped key saw the with_env value plus the append...
        assert_eq!(
            std::fs::read_to_string(keg.join("scoped-inside")).unwrap(),
            "temp more"
        );
        // ...but ensure restored it to the pre-call value.
        assert_eq!(
            std::fs::read_to_string(keg.join("scoped-after")).unwrap(),
            "before"
        );
        // Keys not in the hash keep edits made inside the block.
        assert_eq!(
            std::fs::read_to_string(keg.join("other-inside")).unwrap(),
            "keep:/opt/zb-extra"
        );
        assert_eq!(
            std::fs::read_to_string(keg.join("other-after")).unwrap(),
            "keep:/opt/zb-extra"
        );
    }

    /// Installs a fake "executable" (a /bin/sh script) into bin/ that echoes
    /// back its argv and $SHELL, then reads stdin to EOF.
    const FAKE_COMPLETION_TOOL: &str = r#"
class Foo < Formula
  def install
    FileUtils.mkdir_p(prefix + "bin")
    File.write(prefix + "bin" + "foo", <<~SH)
      #!/bin/sh
      echo "args:$*"
      echo "shell-env:$SHELL"
      cat
    SH
    FileUtils.chmod(0o755, prefix + "bin" + "foo")
    __COMPLETION_CALL__
    File.write(prefix + "installed", "ok")
  end
end
"#;

    async fn run_completions_formula(formula: &str, keg_files: &[&str]) -> tempfile::TempDir {
        let ruby = find_ruby().await.expect("ruby required");
        let tmp = tempfile::tempdir().unwrap();
        let source_root = tmp.path().join("source");
        std::fs::create_dir_all(&source_root).unwrap();

        let prefix = tmp.path().join("prefix");
        run_shim_formula(&ruby, &source_root, &prefix, formula)
            .await
            .unwrap();

        let keg = prefix.join("Cellar").join("foo").join("1.0.0");
        for rel in keg_files {
            assert!(keg.join(rel).exists(), "missing completion file {rel}");
        }
        tmp
    }

    #[tokio::test]
    async fn run_build_generate_completions_writes_all_shells() {
        if find_ruby().await.is_err() {
            return;
        }
        let formula = FAKE_COMPLETION_TOOL.replace(
            "__COMPLETION_CALL__",
            r#"generate_completions_from_executable(bin/"foo", "completions", shells: [:bash, :zsh, :fish])"#,
        );
        let tmp = run_completions_formula(
            &formula,
            &[
                "etc/bash_completion.d/foo",
                "share/zsh/site-functions/_foo",
                "share/fish/vendor_completions.d/foo.fish",
                "installed",
            ],
        )
        .await;

        let keg = tmp.path().join("prefix/Cellar/foo/1.0.0");
        let bash = std::fs::read_to_string(keg.join("etc/bash_completion.d/foo")).unwrap();
        assert!(bash.contains("args:completions bash"), "bash args: {bash}");
        assert!(bash.contains("shell-env:bash"), "bash SHELL env: {bash}");
        let zsh = std::fs::read_to_string(keg.join("share/zsh/site-functions/_foo")).unwrap();
        assert!(zsh.contains("args:completions zsh"), "zsh args: {zsh}");
        let fish =
            std::fs::read_to_string(keg.join("share/fish/vendor_completions.d/foo.fish")).unwrap();
        assert!(fish.contains("args:completions fish"), "fish args: {fish}");
    }

    #[tokio::test]
    async fn run_build_generate_completions_default_shells_and_pwsh() {
        if find_ruby().await.is_err() {
            return;
        }
        // No shells: given -> defaults [:bash, :zsh, :fish].
        let formula = FAKE_COMPLETION_TOOL.replace(
            "__COMPLETION_CALL__",
            r#"generate_completions_from_executable(bin/"foo", "completions")"#,
        );
        run_completions_formula(
            &formula,
            &[
                "etc/bash_completion.d/foo",
                "share/zsh/site-functions/_foo",
                "share/fish/vendor_completions.d/foo.fish",
            ],
        )
        .await;

        // :cobra defaults include pwsh, and the argv parameter is "powershell"
        // while $SHELL stays "pwsh".
        let formula = FAKE_COMPLETION_TOOL.replace(
            "__COMPLETION_CALL__",
            r#"generate_completions_from_executable(bin/"foo", shell_parameter_format: :cobra)"#,
        );
        let tmp = run_completions_formula(
            &formula,
            &[
                "etc/bash_completion.d/foo",
                "share/zsh/site-functions/_foo",
                "share/fish/vendor_completions.d/foo.fish",
                "share/pwsh/completions/_foo.ps1",
            ],
        )
        .await;
        let keg = tmp.path().join("prefix/Cellar/foo/1.0.0");
        let cobra = std::fs::read_to_string(keg.join("etc/bash_completion.d/foo")).unwrap();
        assert!(
            cobra.contains("args:completion bash"),
            "cobra bash args: {cobra}"
        );
        let pwsh = std::fs::read_to_string(keg.join("share/pwsh/completions/_foo.ps1")).unwrap();
        assert!(
            pwsh.contains("args:completion powershell"),
            "cobra pwsh args: {pwsh}"
        );
        assert!(pwsh.contains("shell-env:pwsh"), "pwsh SHELL env: {pwsh}");
    }

    #[tokio::test]
    async fn run_build_generate_completions_shell_parameter_formats() {
        if find_ruby().await.is_err() {
            return;
        }
        let formula = FAKE_COMPLETION_TOOL.replace(
            "__COMPLETION_CALL__",
            r#"
    generate_completions_from_executable(bin/"foo", shell_parameter_format: :arg, shells: [:bash], base_name: "arged")
    generate_completions_from_executable(bin/"foo", shell_parameter_format: :flag, shells: [:zsh], base_name: "flagged")
    generate_completions_from_executable(bin/"foo", shell_parameter_format: "--selected-shell=", shells: [:pwsh], base_name: "prefixed")
    generate_completions_from_executable(bin/"foo", shell_parameter_format: :none, shells: [:fish], base_name: "plain")
    generate_completions_from_executable(bin/"foo", shell_parameter_format: :clap, shells: [:bash], base_name: "clapped")
    generate_completions_from_executable(bin/"foo", shells: ["bash"], base_name: "string-shell")"#,
        );
        let tmp = run_completions_formula(
            &formula,
            &[
                "etc/bash_completion.d/arged",
                "share/zsh/site-functions/_flagged",
                "share/pwsh/completions/_prefixed.ps1",
                "share/fish/vendor_completions.d/plain.fish",
                "etc/bash_completion.d/clapped",
                "etc/bash_completion.d/string-shell",
            ],
        )
        .await;

        let keg = tmp.path().join("prefix/Cellar/foo/1.0.0");
        let read = |p: &str| std::fs::read_to_string(keg.join(p)).unwrap();
        assert!(read("etc/bash_completion.d/arged").contains("args:--shell=bash"));
        assert!(read("share/zsh/site-functions/_flagged").contains("args:--zsh"));
        // A String prefix uses the raw shell symbol, not the powershell mapping.
        assert!(
            read("share/pwsh/completions/_prefixed.ps1").contains("args:--selected-shell=pwsh")
        );
        // :none and :clap add no argv; :clap passes the shell via COMPLETE.
        let plain = read("share/fish/vendor_completions.d/plain.fish");
        assert!(
            plain.contains("args:\n"),
            "none should add no argv: {plain}"
        );
        assert!(read("etc/bash_completion.d/clapped").contains("args:\n"));
        // String shells are accepted (to_sym).
        assert!(read("etc/bash_completion.d/string-shell").contains("args:bash"));
    }

    #[tokio::test]
    async fn run_build_generate_completions_base_name_outside_bin() {
        if find_ruby().await.is_err() {
            return;
        }
        // Executable outside bin/ -> base_name falls back to the formula name.
        let formula = r##"
class Foo < Formula
  def install
    FileUtils.mkdir_p(buildpath + "tools")
    File.write(buildpath + "tools" + "gen", "#!/bin/sh\necho \"args:$*\"\n")
    FileUtils.chmod(0o755, buildpath + "tools" + "gen")
    generate_completions_from_executable(buildpath + "tools" + "gen", shells: [:bash])
  end
end
"##;
        run_completions_formula(formula, &["etc/bash_completion.d/foo"]).await;
    }

    #[tokio::test]
    async fn run_build_generate_completions_failure_aborts_install() {
        let Some(ruby) = find_ruby().await.ok() else {
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let source_root = tmp.path().join("source");
        std::fs::create_dir_all(&source_root).unwrap();

        let formula = r##"
class Foo < Formula
  def install
    FileUtils.mkdir_p(prefix + "bin")
    File.write(prefix + "bin" + "foo", "#!/bin/sh\nexit 3\n")
    FileUtils.chmod(0o755, prefix + "bin" + "foo")
    generate_completions_from_executable(bin/"foo", shells: [:bash])
    File.write(prefix + "installed", "should-not-exist")
  end
end
"##;

        let prefix = tmp.path().join("prefix");
        let err = run_shim_formula(&ruby, &source_root, &prefix, formula)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("failed"),
            "error should report the failing command: {err}"
        );
        let keg = prefix.join("Cellar").join("foo").join("1.0.0");
        assert!(!keg.join("installed").exists(), "install must not continue");
        assert!(
            !keg.join("etc/bash_completion.d/foo").exists(),
            "no completion file on failure"
        );
    }

    #[tokio::test]
    async fn run_build_generate_completions_unspawnable_binary_fails_clearly() {
        let Some(ruby) = find_ruby().await.ok() else {
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let source_root = tmp.path().join("source");
        std::fs::create_dir_all(&source_root).unwrap();

        // ENOENT: the executable does not exist at all.
        let formula = r#"
class Foo < Formula
  def install
    generate_completions_from_executable(bin/"nope", shells: [:bash])
  end
end
"#;
        let prefix = tmp.path().join("prefix");
        let err = run_shim_formula(&ruby, &source_root, &prefix, formula)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("failed"),
            "ENOENT should surface as a clear failure: {err}"
        );

        // EACCES: the file exists but is not executable.
        let formula = r##"
class Foo < Formula
  def install
    FileUtils.mkdir_p(prefix + "bin")
    File.write(prefix + "bin" + "foo", "#!/bin/sh\n")
    generate_completions_from_executable(bin/"foo", shells: [:bash])
  end
end
"##;
        let prefix = tmp.path().join("prefix2");
        let err = run_shim_formula(&ruby, &source_root, &prefix, formula)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("failed"),
            "EACCES should surface as a clear failure: {err}"
        );
    }

    #[tokio::test]
    async fn run_build_generate_completions_stdin_is_dev_null() {
        if find_ruby().await.is_err() {
            return;
        }
        // FAKE_COMPLETION_TOOL's script ends with `cat`: with stdin at
        // /dev/null it reads EOF immediately instead of hanging on the
        // terminal, and the completion file holds only the echoed lines.
        let formula = FAKE_COMPLETION_TOOL.replace(
            "__COMPLETION_CALL__",
            r#"generate_completions_from_executable(bin/"foo", shells: [:bash])"#,
        );
        let tmp = run_completions_formula(&formula, &["etc/bash_completion.d/foo"]).await;
        let keg = tmp.path().join("prefix/Cellar/foo/1.0.0");
        let bash = std::fs::read_to_string(keg.join("etc/bash_completion.d/foo")).unwrap();
        assert!(bash.contains("args:bash"), "cat saw EOF, no hang: {bash}");
    }
}

# frozen_string_literal: true

# This file contains code derived from Homebrew (https://github.com/Homebrew/brew)
# Copyright (c) 2009-present, Homebrew contributors
# Licensed under BSD 2-Clause License (see LICENSE-HOMEBREW)
#
# Portions of this code implement a compatibility shim that mimics Homebrew's
# Formula DSL and helper methods to allow Homebrew formulas to run in ZeroBrew.
#
# Homebrew Compatibility: 5.0.x
# This shim has been tested with Homebrew 5.0.14.
# Last verified: 2025-02-13

require "fileutils"
require "pathname"
require "json"
require "tmpdir"
require "tempfile"
require "digest/sha2"

module ZeroBrewChecksum
  module_function

  def verify_file!(path, expected_sha256, context)
    return if expected_sha256.nil? || expected_sha256.to_s.strip.empty?

    expected = normalize_sha256!(expected_sha256, context)
    actual = Digest::SHA256.file(path).hexdigest
    return if actual == expected

    $stderr.puts "Error: checksum mismatch for #{context} (expected #{expected}, got #{actual})"
    exit 1
  end

  def normalize_sha256!(value, context)
    normalized = value.to_s.strip.downcase
    unless normalized.match?(/\A[0-9a-f]{64}\z/)
      $stderr.puts "Error: invalid sha256 checksum for #{context}: expected 64 hex chars, got #{normalized.length}"
      exit 1
    end
    normalized
  end
end

ZEROBREW_PREFIX = ENV.fetch("ZEROBREW_PREFIX")
ZEROBREW_CELLAR = ENV.fetch("ZEROBREW_CELLAR")
FORMULA_NAME = ENV.fetch("ZEROBREW_FORMULA_NAME")
FORMULA_VERSION = ENV.fetch("ZEROBREW_FORMULA_VERSION")
FORMULA_FILE = ENV.fetch("ZEROBREW_FORMULA_FILE")
INSTALLED_DEPS = JSON.parse(ENV.fetch("ZEROBREW_INSTALLED_DEPS", "{}"))

module OS
  def self.mac?
    RUBY_PLATFORM.include?("darwin")
  end

  def self.linux?
    RUBY_PLATFORM.include?("linux")
  end

  module Mac
    def self.version
      return MacOSVersion.new("15.0") if OS.mac?
      MacOSVersion.new("0")
    end
  end
end

class MacOSVersion
  include Comparable

  def initialize(version)
    @version = version
    @major = version.split(".").first.to_i
  end

  def <=>(other)
    other = MacOSVersion.new(other.to_s) unless other.is_a?(MacOSVersion)
    @version <=> other.to_s
  end

  def to_s; @version; end
  def to_i; @major; end
end

module Hardware
  module CPU
    def self.arm?
      RUBY_PLATFORM.include?("arm") || RUBY_PLATFORM.include?("aarch64")
    end

    def self.intel?
      RUBY_PLATFORM.include?("x86_64")
    end

    def self.is_64_bit?
      true
    end
  end
end

module DevelopmentTools
  def self.ld64_version
    return MacOSVersion.new("0") unless OS.mac?
    raw = `xcrun ld -version_details 2>/dev/null`.strip rescue ""
    m = raw.match(/"version"\s*:\s*"([^"]+)"/)
    MacOSVersion.new(m ? m[1] : "0")
  end
end

module Kernel
  def odie(message)
    $stderr.puts "Error: #{message}"
    exit 1
  end

  # Homebrew Utils::Shell.with_env (utils/shell.rb), exposed as Kernel#with_env
  # (extend/kernel.rb). Snapshot the named keys, yield, then always restore.
  # A nil value unsets the variable for the block. Not thread-safe, matching
  # Homebrew; each formula build runs in its own single-threaded process.
  def with_env(hash)
    old_values = {}
    begin
      hash.each do |key, value|
        key = key.to_s
        old_values[key] = ENV.delete(key)
        ENV[key] = value&.to_s
      end
      yield
    ensure
      ENV.update(old_values)
    end
  end
end

class FormulaVersion < String
  def major
    FormulaVersion.new(split(".")[0] || self)
  end

  def minor
    FormulaVersion.new(split(".")[1] || "0")
  end

  def patch
    FormulaVersion.new(split(".")[2] || "0")
  end

  def major_minor
    FormulaVersion.new("#{major}.#{minor}")
  end

  def to_i
    major.to_s.to_i
  end
end

module Homebrew
  module EnvExtension
    def append(key, value, separator = " ")
      existing = self[key]
      self[key] = existing && !existing.empty? ? "#{existing}#{separator}#{value}" : value.to_s
    end

    def prepend(key, value, separator = " ")
      existing = self[key]
      self[key] = existing && !existing.empty? ? "#{value}#{separator}#{existing}" : value.to_s
    end

    def append_path(key, path)
      append(key, path, ":")
    end

    def prepend_path(key, path)
      prepend(key, path, ":")
    end

    def prepend_create_path(key, path)
      FileUtils.mkdir_p(path.to_s)
      prepend(key, path, ":")
    end
  end
end
ENV.extend(Homebrew::EnvExtension)

class PatchDSL
  attr_reader :patch_url, :patch_sha256

  def initialize
    @patch_url = nil
    @patch_sha256 = nil
  end

  def url(u, **_kwargs); @patch_url = u; end
  def sha256(s); @patch_sha256 = s; end
  def mirror(_); nil; end
end

class ResourceDSL
  attr_reader :resource_url, :resource_sha256

  def initialize(name)
    @name = name
    @resource_url = nil
    @resource_sha256 = nil
  end

  def url(u, **_kwargs); @resource_url = u; end
  def sha256(s); @resource_sha256 = s; end
  def mirror(_); nil; end
  def patch(&_block); nil; end
  def on_macos(&block); yield if OS.mac?; end
  def on_linux(&block); yield if OS.linux?; end
  def on_arm(&block); yield if Hardware::CPU.arm?; end
  def on_intel(&block); yield if Hardware::CPU.intel?; end
end

class StagedResource
  def initialize(url, sha256)
    @url = url
    @sha256 = sha256
  end

  def stage(&block)
    Dir.mktmpdir("zb_resource_") do |dir|
      basename = File.basename(URI.parse(@url).path) rescue "resource.tar.gz"
      archive = File.join(dir, basename)
      Kernel.system("curl", "-sSL", "-o", archive, @url)
      unless $?.success?
        $stderr.puts "Error: failed to download resource #{@url}"
        exit 1
      end
      ZeroBrewChecksum.verify_file!(archive, @sha256, "resource #{@url}")
      extract_resource(archive, dir)
      entries = Dir.children(dir).reject { |e| e == basename }
      src_dir = if entries.length == 1 && File.directory?(File.join(dir, entries.first))
                  File.join(dir, entries.first)
                else
                  dir
                end
      if block_given?
        Dir.chdir(src_dir) { block.call(Pathname.new(src_dir)) }
      else
        Pathname.new(src_dir)
      end
    end
  end

  private

  def extract_resource(archive, dir)
    case archive
    when /\.(tar\.gz|tgz)$/
      Kernel.system("tar", "xzf", archive, "-C", dir)
    when /\.tar\.xz$/
      Kernel.system("tar", "xJf", archive, "-C", dir)
    when /\.tar\.bz2$/
      Kernel.system("tar", "xjf", archive, "-C", dir)
    when /\.zip$/
      Kernel.system("unzip", "-qo", archive, "-d", dir)
    else
      Kernel.system("tar", "xf", archive, "-C", dir)
    end
  end
end

class BuildOptions
  def head?; false; end
  def stable?; true; end
  def with?(name); false; end
  def without?(name); true; end
end

class Pathname
  def install(*sources)
    sources.flatten.each do |src|
      if src.is_a?(Hash)
        src.each { |from, to| install_renamed(from, to) }
        next
      end
      src = Pathname.new(src) unless src.is_a?(Pathname)
      if src.directory?
        dst = self + src.basename
        FileUtils.mkdir_p(dst)
        Dir.children(src.to_s).each { |child| dst.install(src + child) }
      else
        FileUtils.mkdir_p(self)
        FileUtils.cp(src.to_s, self.to_s)
      end
    end
  end

  def install_symlink(*sources)
    sources.flatten.each do |src|
      if src.is_a?(Hash)
        src.each do |from, to|
          FileUtils.mkdir_p(self)
          FileUtils.ln_sf(Pathname.new(from).expand_path.to_s, (self + to.to_s).to_s)
        end
        next
      end
      src = Pathname.new(src) unless src.is_a?(Pathname)
      FileUtils.mkdir_p(self)
      FileUtils.ln_sf(src.expand_path.to_s, (self + src.basename).to_s)
    end
  end

  def write(content)
    FileUtils.mkdir_p(dirname)
    File.write(to_s, content)
  end

  def unlink
    FileUtils.rm_f(to_s)
  end

  private

  def install_renamed(from, to)
    from = Pathname.new(from) unless from.is_a?(Pathname)
    FileUtils.mkdir_p(self)
    FileUtils.cp(from.to_s, (self + to.to_s).to_s)
  end
end

class Formula
  class << self
    attr_accessor :formula_name, :formula_version

    def desc(_); nil; end
    def homepage(_); nil; end
    def license(_); nil; end
    def url(*_args, **_kwargs); nil; end
    def sha256(_); nil; end
    def revision(_); nil; end
    def compatibility_version(_); nil; end
    def mirror(_); nil; end

    def option(*_args); nil; end
    def build; BuildOptions.new; end

    def depends_on(_); nil; end
    def uses_from_macos(*_args, **_kwargs); nil; end
    def on_macos(&block); yield if OS.mac?; end
    def on_linux(&block); yield if OS.linux?; end
    def on_intel(&block); yield if Hardware::CPU.intel?; end
    def on_arm(&block); yield if Hardware::CPU.arm?; end
    def on_system(*args, **kwargs, &block)
      return unless block_given?
      if OS.linux?
        yield if args.include?(:linux)
      elsif OS.mac?
        yield if args.include?(:macos) || kwargs.key?(:macos)
      end
    end

    def head(*_args, **_kwargs, &_block); nil; end
    def no_autobump!(**_kwargs); nil; end
    def livecheck(&block); nil; end
    def bottle(&block); nil; end
    def skip_clean(*_args); nil; end
    def link_overwrite(*_args); nil; end
    def keg_only(*_args); nil; end
    def caveats; nil; end
    def test(&block); nil; end
    def service(&block); nil; end

    def resource(name, &block)
      return unless block_given?
      @_resources ||= {}
      ctx = ResourceDSL.new(name)
      ctx.instance_eval(&block)
      @_resources[name.to_s] = { url: ctx.resource_url, sha256: ctx.resource_sha256 }
    end

    def patch(*args, &block)
      @_patches ||= []
      strip = :p1
      data_patch = false

      args.each do |arg|
        case arg
        when :DATA
          data_patch = true
        when Symbol
          strip = arg if arg.to_s.match?(/\Ap\d+\z/)
        when String
          @_patches << { type: :inline, content: arg, strip: strip }
          return
        end
      end

      if data_patch
        @_patches << { type: :data, strip: strip }
      elsif block_given?
        ctx = PatchDSL.new
        ctx.instance_eval(&block)
        @_patches << { type: :url, url: ctx.patch_url, sha256: ctx.patch_sha256, strip: strip }
      end
    end

    def [](name)
      FormulaRef.new(name)
    end

    def inherited(subclass)
      subclass.formula_name = FORMULA_NAME
      subclass.formula_version = FORMULA_VERSION
      subclass.instance_variable_set(:@_patches, [])
      subclass.instance_variable_set(:@_resources, {})
    end
  end

  def name; self.class.formula_name; end
  def version; FormulaVersion.new(self.class.formula_version); end
  def build; BuildOptions.new; end

  def prefix
    Pathname.new(ZEROBREW_CELLAR) + name + version
  end

  def bin; prefix + "bin"; end
  def sbin; prefix + "sbin"; end
  def lib; prefix + "lib"; end
  def libexec; prefix + "libexec"; end
  def include; prefix + "include"; end
  def share; prefix + "share"; end
  def man; share + "man"; end
  def man1; man + "man1"; end
  def man2; man + "man2"; end
  def man3; man + "man3"; end
  def man4; man + "man4"; end
  def man5; man + "man5"; end
  def man6; man + "man6"; end
  def man7; man + "man7"; end
  def man8; man + "man8"; end
  def doc; share + "doc" + name; end
  def info; share + "info"; end
  def pkgshare; share + name; end
  def frameworks; prefix + "Frameworks"; end
  def kext; prefix + "Library" + "Extensions"; end

  def opt_prefix; Pathname.new(ZEROBREW_PREFIX) + "opt" + name; end
  def opt_bin; opt_prefix + "bin"; end
  def opt_sbin; opt_prefix + "sbin"; end
  def opt_lib; opt_prefix + "lib"; end
  def opt_include; opt_prefix + "include"; end
  def opt_share; opt_prefix + "share"; end
  def opt_pkgshare; opt_prefix + "share" + name; end

  def bash_completion; prefix + "etc" + "bash_completion.d"; end
  def zsh_completion; share + "zsh" + "site-functions"; end
  def fish_completion; share + "fish" + "vendor_completions.d"; end
  def pwsh_completion; share + "pwsh" + "completions"; end
  def elisp; share + "emacs" + "site-lisp" + name; end

  def resource(name)
    res_info = self.class.instance_variable_get(:@_resources)&.dig(name.to_s)
    raise "Resource '#{name}' not defined" unless res_info
    StagedResource.new(res_info[:url], res_info[:sha256])
  end

  def etc
    Pathname.new(ZEROBREW_PREFIX) + "etc"
  end

  def var
    Pathname.new(ZEROBREW_PREFIX) + "var"
  end

  def buildpath
    Pathname.new(Dir.pwd)
  end

  def testpath
    buildpath + "test"
  end

  def inreplace(paths, before = nil, after = nil, &block)
    Array(paths).each do |path|
      content = File.read(path.to_s)
      if block_given?
        s = InreplaceString.new(content)
        block.call(s)
        content = s.to_s
      else
        content = content.gsub(before.to_s, after.to_s)
      end
      File.write(path.to_s, content)
    end
  end

  def system(*args)
    cmd = args.map(&:to_s).join(" ")
    puts "==> #{cmd}"
    result = Kernel.system(*args.map(&:to_s))
    exit 1 unless result
  end

<<<<<<< HEAD
  # Homebrew Formula#generate_completions_from_executable (formula.rb) via
  # Utils::ShellCompletion (utils/shell_completion.rb). Runs the executable
  # once per shell and writes its stdout to the completion script path.
  # stdin is /dev/null and the parent environment is inherited (so with_env
  # applies), matching Homebrew's Utils.popen_read. Failures abort the build
  # like Formula#system does; nothing is written unless the run succeeds.
  def generate_completions_from_executable(
    *commands, base_name: nil, shell_parameter_format: nil, shells: nil
  )
    if commands.empty?
      $stderr.puts "Error: generate_completions_from_executable requires a command"
      exit 1
    end

    shells ||= case shell_parameter_format
               when :cobra, :typer then [:bash, :zsh, :fish, :pwsh]
               else [:bash, :zsh, :fish]
               end

    executable = commands.first.to_s
    base_name ||= File.basename(executable) if executable.start_with?(bin.to_s, sbin.to_s)
    base_name ||= name

    paths = {
      bash: bash_completion + base_name,
      zsh: zsh_completion + "_#{base_name}",
      fish: fish_completion + "#{base_name}.fish",
      pwsh: pwsh_completion + "_#{base_name}.ps1",
    }

    shells.each do |shell|
      shell = shell.to_sym
      script_path = paths[shell]
      unless script_path
        $stderr.puts "Error: unsupported completion shell: #{shell}"
        exit 1
      end

      # Go's cobra and Rust's clap accept "powershell"; SHELL stays "pwsh".
      shell_parameter = (shell == :pwsh) ? "powershell" : shell.to_s
      env = { "SHELL" => shell.to_s }
      extra = case shell_parameter_format
              when nil then [shell_parameter]
              when :arg then ["--shell=#{shell_parameter}"]
              when :clap
                env["COMPLETE"] = shell_parameter
                nil
              when :click
                prog_name = File.basename(executable).upcase.tr("-", "_")
                env["_#{prog_name}_COMPLETE"] = "#{shell_parameter}_source"
                nil
              when :cobra then ["completion", shell_parameter]
              when :flag then ["--#{shell_parameter}"]
              when :none then nil
              when :typer
                env["_TYPER_COMPLETE_TEST_DISABLE_SHELL_DETECTION"] = "1"
                ["--show-completion", shell_parameter]
              # A String format is a parameter prefix; it uses the raw shell
              # symbol, not the powershell-mapped name.
              else ["#{shell_parameter_format}#{shell}"]
              end

      argv = commands.map(&:to_s) + Array(extra)
      begin
        output = IO.popen([env, *argv], "rb", in: File::NULL) { |io| io.read }
        status = $?
      rescue SystemCallError => e
        $stderr.puts "Error: #{argv.join(" ")} failed: #{e.message}"
        exit 1
      end
      unless status.success?
        $stderr.puts "Error: #{argv.join(" ")} failed (#{status})"
        exit 1
      end

      script_path.write(output)
    end

  def cd(dir, &block)
    dir = dir.respond_to?(:to_path) ? dir.to_path : dir.to_s
    puts "==> cd #{dir}"
    block_given? ? Dir.chdir(dir, &block) : Dir.chdir(dir)
  end

  def mv(*sources, **options)
    normalized = sources.map { |s| s.respond_to?(:to_path) ? s.to_path : s.to_s }
    FileUtils.mv(*normalized, **options)
  end

  def std_configure_args
    ["--disable-debug", "--disable-dependency-tracking", "--prefix=#{prefix}", "--libdir=#{lib}"]
  end

  def std_cmake_args
    [
      "-DCMAKE_INSTALL_PREFIX=#{prefix}",
      "-DCMAKE_INSTALL_LIBDIR=lib",
      "-DCMAKE_BUILD_TYPE=Release",
      "-DCMAKE_FIND_FRAMEWORK=LAST",
      "-DCMAKE_VERBOSE_MAKEFILE=ON",
      "-DBUILD_TESTING=OFF",
    ]
  end

  def std_meson_args
    ["--prefix=#{prefix}", "--libdir=lib", "--buildtype=release", "--wrap-mode=nofallback"]
  end

  def std_go_args(**overrides)
    ldflags = overrides.fetch(:ldflags, "")
    output = overrides.fetch(:output, bin + name)
    args = ["build", "-trimpath", "-o=#{output}"]
    args << "-ldflags=#{ldflags}" unless ldflags.empty?
    args
  end

  def self.method_missing(method_name, *args, &block)
    nil
  end

  def self.respond_to_missing?(method_name, include_private = false)
    true
  end
end

class FormulaRef
  def initialize(name)
    @name = name.to_s
  end

  def opt_prefix
    dep_info = INSTALLED_DEPS[@name]
    return Pathname.new(ZEROBREW_PREFIX) + "opt" + @name if dep_info
    Pathname.new(ZEROBREW_PREFIX) + "opt" + @name
  end

  def opt_lib; opt_prefix + "lib"; end
  def opt_include; opt_prefix + "include"; end
  def opt_bin; opt_prefix + "bin"; end
  def lib; opt_lib; end
  def include; opt_include; end

  def prefix
    dep_info = INSTALLED_DEPS[@name]
    return Pathname.new(dep_info["cellar_path"]) if dep_info
    opt_prefix
  end

  def any_installed_version; self; end

  def to_s; @name; end
end

class InreplaceString
  def initialize(content)
    @content = content
  end

  def gsub!(pattern, replacement)
    @content = @content.gsub(pattern, replacement)
  end

  def sub!(pattern, replacement)
    @content = @content.sub(pattern, replacement)
  end

  def change_make_var!(name, value)
    @content = @content.gsub(/^#{Regexp.escape(name)}\s*[?:]?=.*$/, "#{name}=#{value}")
  end

  def remove_make_var!(name)
    @content = @content.gsub(/^#{Regexp.escape(name)}\s*[?:]?=.*$\n?/, "")
  end

  def to_s; @content; end
end

module Language
  module Python
    def self.major_minor_version(python)
      raw = `#{python} --version 2>&1`.strip.split.last
      parts = raw.split(".")
      "#{parts[0]}.#{parts[1]}"
    end
  end
end

def shared_library(name, version = nil)
  if OS.mac?
    version ? "#{name}.#{version}.dylib" : "#{name}.dylib"
  else
    version ? "#{name}.so.#{version}" : "#{name}.so"
  end
end

formula_raw = File.read(FORMULA_FILE)
end_marker_idx = formula_raw.index(/^__END__\s*$/)
FORMULA_DATA_CONTENT = end_marker_idx ? formula_raw[(formula_raw.index("\n", end_marker_idx) + 1)..] : nil

ENV["HOMEBREW_PREFIX"] = ZEROBREW_PREFIX
ENV["HOMEBREW_CELLAR"] = ZEROBREW_CELLAR

load FORMULA_FILE

formula_class = ObjectSpace.each_object(Class).find { |c| c < Formula && c != Formula }
unless formula_class
  $stderr.puts "Error: no formula class found in #{FORMULA_FILE}"
  exit 1
end

patches = formula_class.instance_variable_get(:@_patches) || []
patches.each do |p|
  strip_flag = "-#{p[:strip]}"
  case p[:type]
  when :data
    if FORMULA_DATA_CONTENT
      puts "==> Applying DATA patch"
      IO.popen(["patch", strip_flag, "-i", "/dev/stdin"], "w") { |io| io.write(FORMULA_DATA_CONTENT) }
      unless $?.success?
        $stderr.puts "Error: DATA patch failed"
        exit 1
      end
    end
  when :url
    puts "==> Downloading patch from #{p[:url]}"
    tmp = Tempfile.new("zb_patch")
    begin
      Kernel.system("curl", "-sSL", "-o", tmp.path, p[:url])
      unless $?.success?
        $stderr.puts "Error: failed to download patch #{p[:url]}"
        exit 1
      end
      ZeroBrewChecksum.verify_file!(tmp.path, p[:sha256], "patch #{p[:url]}")
      Kernel.system("patch", strip_flag, "-i", tmp.path)
      unless $?.success?
        $stderr.puts "Error: patch failed"
        exit 1
      end
    ensure
      tmp.close!
    end
  when :inline
    puts "==> Applying inline patch"
    IO.popen(["patch", strip_flag, "-i", "/dev/stdin"], "w") { |io| io.write(p[:content]) }
    unless $?.success?
      $stderr.puts "Error: inline patch failed"
      exit 1
    end
  end
end

instance = formula_class.new

puts "==> Building #{FORMULA_NAME} #{FORMULA_VERSION}"
FileUtils.mkdir_p(instance.prefix.to_s)
instance.install
puts "==> Build complete: #{FORMULA_NAME} #{FORMULA_VERSION}"

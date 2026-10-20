class Hivemind < Formula
  desc "Run CLI agents on several Macs over the home WiFi"
  homepage "https://github.com/akhil-gautam/hivemind"
  # The repository is public, so the source is cloned over plain HTTPS: no
  # token, no SSH key, no credential helper.
  url "https://github.com/akhil-gautam/hivemind.git", tag: "v0.4.0"
  version "0.4.0"
  license "MIT"

  def install
    # Homebrew builds with a trimmed PATH that hides brew itself, which the
    # rustc shim needs to find the real compiler.
    ENV["PATH"] = "#{HOMEBREW_PREFIX}/bin:#{ENV["PATH"]}"

    candidates = [
      ENV["CARGO"],
      File.join(HOMEBREW_PREFIX.to_s, "bin", "cargo"),
      File.join(Dir.home, ".cargo", "bin", "cargo"),
    ].compact
    candidates.concat(ENV["PATH"].to_s.split(File::PATH_SEPARATOR).map { |dir| File.join(dir, "cargo") })
    cargo = candidates.find { |path| File.executable?(path) }
    raise "cargo not found — install Rust first (brew install rust)" if cargo.nil?

    # Keep the build out of the user's ~/.cargo
    ENV["CARGO_HOME"] = buildpath/".cargo-home"
    system cargo, "install", "--path", ".", "--locked", "--root", buildpath/"cargo-install"
    bin.install Dir["#{buildpath}/cargo-install/bin/hivemind"]
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/hivemind version")
  end
end

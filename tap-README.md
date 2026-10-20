# homebrew-hivemind

Homebrew tap for [hivemind](https://github.com/akhil-gautam/hivemind) — run CLI
agents (Claude Code, OpenCode, Codex, ollama, …) on several Macs over the home
WiFi and drive them from one browser page.

```sh
brew tap akhil-gautam/hivemind git@github.com:akhil-gautam/homebrew-hivemind.git
brew install akhil-gautam/hivemind/hivemind
```

Things worth knowing:

* **Use the fully qualified name.** homebrew-core already ships an unrelated
  `hivemind` (a Procfile process manager), so plain `brew install hivemind`
  installs that one instead.
* **Everything is private**, so Homebrew clones the tap *and* the source over
  SSH. You need a GitHub SSH key with access to this repository — the same
  key that clones the tap is used for the source. Without one the install
  stops with `could not read Username for 'https://github.com'`. If you
  prefer a token to a key, run `gh auth setup-git` once before installing.
* **Rust is required** to build from source: `brew install rust`.

Upgrades:

```sh
brew update
brew upgrade akhil-gautam/hivemind/hivemind
```

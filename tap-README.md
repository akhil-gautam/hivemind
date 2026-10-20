# homebrew-hivemind

Homebrew tap for [hivemind](https://github.com/akhil-gautam/hivemind) — run CLI
agents (Claude Code, OpenCode, Codex, ollama, …) on several Macs over the home
WiFi and drive them from one browser page.

```sh
brew tap akhil-gautam/hivemind
brew install akhil-gautam/hivemind/hivemind
```

Things worth knowing:

* **Use the fully qualified name.** homebrew-core already ships an unrelated
  `hivemind` (a Procfile process manager), so plain `brew install hivemind`
  installs that one instead.
* **Rust is required** to build from source: `brew install rust`.
* **A node needs the access key** before it will do any work. The page asks
  for it the first time you open it; you can also set it from a terminal with
  `hivemind license HM-4KD9-7QXR-2ZTM`.

Everything is public, so no token, no SSH key and no credentials are needed.

Upgrades:

```sh
brew update
brew upgrade akhil-gautam/hivemind/hivemind
```

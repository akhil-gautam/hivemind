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
* **Everything is private.** The SSH URL in the `brew tap` line lets Homebrew
  clone this tap with your GitHub SSH key. The formula then fetches the
  source over HTTPS, which Homebrew authenticates with the token from the
  GitHub CLI — run `gh auth setup-git` once if you have not already. If you
  would rather use HTTPS, export `HOMEBREW_GITHUB_API_TOKEN=$(gh auth token)`
  and tap without the URL.
* **Rust is required** to build from source: `brew install rust`.

Upgrades:

```sh
brew update
brew upgrade akhil-gautam/hivemind/hivemind
```

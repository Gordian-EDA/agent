# Security Policy

## Reporting a vulnerability

Please report security issues privately rather than opening a public issue. Use GitHub's
[private vulnerability reporting](https://github.com/Gordian-EDA/agent/security/advisories/new)
(Security → Report a vulnerability), or contact the maintainers directly.

We will acknowledge your report, investigate, and coordinate a fix and disclosure timeline with you.

## Scope

This project shells out to `kicad-cli` and a bundled Freerouting JVM, reads KiCad's symbol/footprint
libraries, and sends prompts to a configured LLM provider. **API keys live only in your local
`config.toml`** (`~/.config/gordian/config.toml`) — never commit them. Reports about credential
handling, command injection via untrusted circuit input, or unsafe file writes are especially
welcome.

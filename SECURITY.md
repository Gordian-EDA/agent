# Security Policy

## Reporting a vulnerability

Please report security issues privately rather than opening a public issue. Use GitHub's
[private vulnerability reporting](https://github.com/Gordian-EDA/agent/security/advisories/new)
(Security → Report a vulnerability), or contact the maintainers directly.

We will acknowledge your report, investigate, and coordinate a fix and disclosure timeline with you.

## Scope

This project shells out to `kicad-cli` and reads KiCAD's symbol/footprint libraries, and (for the
agent loop) sends prompts to a configured LLM provider. Note that **API keys live only in your local
`.env`** — never commit them. Reports about credential handling, command injection via untrusted
circuit input, or unsafe file writes are especially welcome.

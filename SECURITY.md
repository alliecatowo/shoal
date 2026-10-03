# Security policy

Shoal is pre-release software that executes untrusted input (scripts, agent requests over MCP, WASM
plugins) and enforces a capability model (Leash) with OS sandboxing. Security reports are welcome.

## Supported versions

Only the latest `0.1.x` release receives security fixes. Please reproduce on the latest release or
on `main` before reporting.

## Reporting a vulnerability

Please do not open a public issue. Use GitHub's private vulnerability reporting:
<https://github.com/alliecatowo/shoal/security/advisories/new>

Include the affected version or commit, a minimal reproduction, and the impact you observed. You can
expect an acknowledgement within a few days. Fixes ship in a patch release, and reporters are
credited unless they prefer otherwise.

## Scope and threat model

What Leash, the kernel, the sandbox, and the MCP facade do and do not promise is documented in the
[security threat model](site/content/internals/security-threat-model.md). In particular, a
vulnerability is a way for a principal to exceed its policy (for example running a command, reading
a file, or reading an environment variable its grants forbid), for untrusted project files to
execute code without `shoal trust`, or for a secret to leak. Known limitations listed in the threat
model are not vulnerabilities by themselves.

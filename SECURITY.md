# Security Policy

## Supported Versions

Only the latest release receives security fixes.

## Reporting a Vulnerability

Please **do not** open a public GitHub issue for security-sensitive
problems. Instead use one of the private channels below:

1. **Preferred:** GitHub Security Advisories - go to
   <https://github.com/nihen/ah/security/advisories/new> and submit a
   private report.
2. **Email:** <nihen@megabbs.com>. Use a clear subject line such as
   `[ah security] <one-line summary>`.

We aim to acknowledge new reports within **3 business days** and to
provide a remediation plan or patch timeline within **14 days**.

When reporting, please include:

- Affected version (`ah --version`) and platform.
- A minimal reproducer: the command (or MCP tool call) and, where it
  matters, a small session or configuration file that triggers it.
- Observed behavior vs. expected, and any impact assessment you have.

We do not run a paid bug bounty programme. We are happy to credit
reporters in release notes unless you prefer to remain anonymous.

## What ah trusts

`ah` reads the session history of coding agents, which records content
from repositories, tools and web pages the agents worked with. It treats
that content as untrusted:

- Terminal output strips escape sequences and control characters from
  session data (see README › Control characters in session data);
  `ah show --raw` is the only verbatim output.
- Project memory files are read only when they resolve inside their
  repository or to a global memory file (README › memory).
- `ah resume` passes session ids to agent CLIs only when they cannot be
  read as options, and quotes remote commands for the remote shell.

Configuration (`~/.ahrc`), the agents' own global settings, and remote
hosts listed in `~/.ahrc` are trusted: `ah` runs `ssh` and agent CLIs as
they say.

## Verifying release binaries

Binaries of releases after v0.8.0 on the
[Releases](https://github.com/nihen/ah/releases) page have a signed build
provenance attestation from this repository's release workflow. Check one
with the [GitHub CLI](https://cli.github.com/) 2.102.0 or later (earlier
versions match `--signer-workflow` and `--source-ref` too loosely):

```bash
TAG=v0.9.0   # the release you downloaded
gh attestation verify ah-linux-x86_64 --repo nihen/ah \
  --signer-workflow nihen/ah/.github/workflows/release.yml \
  --source-ref "refs/tags/$TAG"
```

Keep both options: `--repo` alone also accepts an attestation made by
another workflow or from another ref of this repository.

The Homebrew formula is generated from binaries verified this way, and
the crate is published to crates.io with trusted publishing.

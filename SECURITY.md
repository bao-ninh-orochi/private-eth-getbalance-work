# Security Policy

## Scope and status

This repository is a proof-of-concept **private `eth_getBalance` over
RisePIR** — a research artifact, not an audited product. It runs against real
mainnet, but it has **not** been independently audited and must not be
treated as production privacy infrastructure.

Read [`docs/threat-model.md`](docs/threat-model.md) before reporting: it
states precisely what is defended, what is detected, and what is *knowingly*
undefended. In particular, the following are **documented limitations, not
vulnerabilities**:

- a malicious PIR operator can forge balances that pass every client check
  (threat model §4.2 — the stated trust assumption);
- network-layer metadata (that you queried, when, from where) is not hidden
  (§5), and the current deployment is plaintext HTTP;
- feed poisoning is detected by sampled reconciliation with bounded lag, not
  prevented (§6);
- volumetric denial of service is only partially mitigated (§3).

Reports that *break a stated guarantee* are the valuable ones — above all
anything that makes the system **return a wrong answer silently** (the
repo's first binding rule), leak which address a query targets, or panic /
allocate unboundedly on attacker-controlled input.

## Reporting a vulnerability

Please **do not open a public issue** for security-relevant findings.
Instead, use one of:

- GitHub's private vulnerability reporting on this repository
  ("Report a vulnerability" under the Security tab), or
- email the maintainer: <bao.ninh@orochi.network>.

## Supported versions

Pre-1.0: only the `main` branch receives fixes. Response and remediation are
best-effort; there is no SLA while the project is a research artifact.

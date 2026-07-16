# Security policy

Only the latest release receives security fixes.

Please report vulnerabilities privately through the repository's
[security advisory form](https://github.com/randomvibecoder/subagent/security/advisories/new).
Do not open a public issue for an undisclosed vulnerability.

`subagent` is a host-native automation tool, not a sandbox. Agents inherit the daemon
user's filesystem, process, credential, and network access. Readonly mode withholds
structured write tools but Bash remains capable of changing host state. The optional
Web UI listens only on localhost; set `SUBAGENT_WEB_PASSWORD` when another local user
or forwarded connection could reach it.

Reports about an agent doing something its host user was already permitted to do are
outside the security boundary unless the daemon crossed an explicitly documented
authorization or isolation boundary.
